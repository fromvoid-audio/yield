//! Asynchronous ownership for the profile-local operation journal.
//!
//! The owner moves an already-open [`OperationJournalStore`] onto one worker thread. Its client
//! handles contain only bounded command and result state; they do not retain a profile guard or a
//! SQLite connection. A successful queue submission means that storage work was admitted to this
//! worker. It does not grant operation admission, filesystem authority, capacity authority, or a
//! tentative persistence receipt.

pub use super::wal_admission::{
    SourceInput as SourceWalPreparationInput, SourcePreparation as WalPreparationSource,
    WalPreparationError, WalPreparationHandle, WalPreparationInput, WalPreparationObservation,
    WalPreparationStatus, WalPreparedDatabase,
};
mod control_credit;
mod frozen_copy;
mod historical_reply;
mod namespace_credit;
#[cfg(test)]
pub(in crate::operation_journal) use namespace_credit::NoEffectSlotFixture;
pub use namespace_credit::RetainedCommandError;
mod startup_credit;
use super::store::FundedChunkClaimRecord;
use historical_reply::HistoricalReply;
#[cfg(any(test, feature = "test-support"))]
pub use namespace_credit::JournalNamespaceFundingProbe;
pub(super) use startup_credit::StartupCredit;
mod queue_metrics;
pub use queue_metrics::{JournalQueueClassMetrics, JournalQueueMetricsSnapshot};
use queue_metrics::{QueueMetrics, QueueTicket};
#[cfg(all(test, unix))]
mod current_source_custody_tests;
mod current_source_metadata;
#[cfg(all(test, unix))]
mod current_source_native_fault_tests;
mod legacy_admission;
mod legacy_adoption_custody;
mod legacy_adoption_volumes;
mod recording_audit;
mod recording_binding;
pub use legacy_adoption_custody::{LegacyAdoptionCustody, LegacyAdoptionCustodyError};
pub use recording_binding::{RecordingProducerCustody, RecordingProducerCustodyError};
mod native_read_capsule;
mod publication_charge;
mod publication_custody;
mod publication_fence;
pub(crate) use publication_fence::NoEffectCompletion;
mod resource_custody;
mod retirement_admission;
pub use frozen_copy::{FrozenCopyObservation, FrozenCopyValidationError};
use native_read_capsule::NativeCapsuleData;
use resource_custody::ResourceAdmission;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::panic::{self, AssertUnwindSafe};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use crate::app_dirs::WritableProfileGuard;
use crate::durable_capacity::{
    PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity, descriptor_capacity_facts,
};
use crate::file_operation_owner::{
    EffectId, EffectRequest, FileOperationCommand, FileOperationResult, OutputIdentity,
    PreparedPublicationTarget, RootRegistrationRef, SourceFence,
};
use crate::sample_sources::db::{
    SourceAuthorityDrainMarker, SourceAuthorityError, SourceAuthorityRegistrationReceipt,
    SourceAuthorityRegistrationRequest, SourceAuthorityRequest, SourceAuthorityRetirementAck,
    SourceEffectPermit, SourceEffectPin,
};

use super::admission_claims::{AdmissionClaimId, AdmissionClaimInventory};
use super::capacity_provider::{
    self, CapacityProvider, CapacityProviderError, CapacityRebuildReadiness,
};
use super::capacity_reserve_namespace::{OwnedReserveAllocation, PhysicalReserveProof};
use super::capacity_volume_owner::VolumeOwnershipCapability;
use super::codec::{CappedWriter, DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::coordinator::{RetryObservation, UniversalCommandArgs};
use super::full_capacity::FullCapacityPlan;
use super::full_progress_codec::{
    FullProgressCodecError, StoredFullProgressRecord, decode as decode_full_progress,
    encode as encode_full_progress,
};
use super::model::{
    CancellationRequestObservation, MAX_RECORD_BYTES, OperationDisposition, OperationId,
    OperationIntent, UniversalIntent,
};
use super::participants::ParticipantSet;
use super::recording_commands::{RecordingCapturedChunk, RecordingProposal};
use super::store::recovery::{
    CoordinatorRecoveryPage, RecoveryCursor, RecoveryPage, RecoveryScanError,
};
use super::store::{
    ChunkClaimRequest, ChunkClaimState, ChunkId, ChunkIssueReceipt, ChunkStagingReceipt,
    ChunkWriteIdentity, FullProgressWriteOutcome, InitialClaimSettlementError,
    JournalCancelRequestReceipt, JournalInsertFailure, JournalInsertOutcome, JournalInsertReceipt,
    JournalInsertWriteStage, JournalRetryReceipt, OperationJournalStore,
    OperationJournalStoreError, OrdinaryClaimReleaseReceipt, RecordingChunkPage,
    RecordingChunkReceipt, SourceReserveBindingFailure, StagingCheckpointAck,
};
#[cfg(all(test, target_os = "macos"))]
use super::store::{ReleaseTestBoundary, ReleaseTestDirective};
use super::work_plan::derive_work_plan;

const CONTROL_INPUT_BYTES: usize = 1;
// Disjoint credit classes on the original four-MiB pool; no ordinary caller can borrow control.
const CONTROL_INPUT_RESERVED_BYTES: usize = 64 * 1024;
const PUBLICATION_INPUT_BYTES: usize = 2 * 1024 * 1024;
const ORDINARY_INPUT_BYTES: usize =
    OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES - CONTROL_INPUT_RESERVED_BYTES;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputClass {
    Ordinary,
    Control,
}
impl InputClass {
    fn index(self) -> usize {
        match self {
            Self::Ordinary => 0,
            Self::Control => 1,
        }
    }
    fn limit(self) -> usize {
        match self {
            Self::Ordinary => ORDINARY_INPUT_BYTES,
            Self::Control => CONTROL_INPUT_RESERVED_BYTES,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputBudget {
    remaining: [usize; 2],
    publication_remaining: usize,
    unavailable: bool,
}
impl InputBudget {
    fn new() -> Self {
        Self {
            remaining: [ORDINARY_INPUT_BYTES, CONTROL_INPUT_RESERVED_BYTES],
            publication_remaining: PUBLICATION_INPUT_BYTES,
            unavailable: false,
        }
    }
    fn valid(&self) -> bool {
        !self.unavailable
            && self.remaining[0] <= ORDINARY_INPUT_BYTES - startup_credit::POOL_BYTES
            && self.remaining[1] <= CONTROL_INPUT_RESERVED_BYTES
            && self.publication_remaining <= PUBLICATION_INPUT_BYTES
            && PUBLICATION_INPUT_BYTES - self.publication_remaining
                <= ORDINARY_INPUT_BYTES - startup_credit::POOL_BYTES - self.remaining[0]
    }
}

#[cfg(all(test, target_os = "macos"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedAdmissionFactsFault {
    LowReserve,
    DiskPressure,
    DiskPressureBoundary,
    ExactBoundary,
    Unsupported,
    Replaced,
}

/// Aggregate queued-command limit, privately split into 28 ordinary and four control entries.
pub const OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY: usize = 32;

/// Aggregate outstanding-result limit, privately split into 60 ordinary and four control entries.
pub const OPERATION_JOURNAL_OWNER_RESULT_CAPACITY: usize = 64;

const CONTROL_COMMAND_CAPACITY: usize = 4;
const ORDINARY_COMMAND_CAPACITY: usize =
    OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY - CONTROL_COMMAND_CAPACITY;
const CONTROL_RESULT_CAPACITY: usize = 4;
const ORDINARY_RESULT_CAPACITY: usize =
    OPERATION_JOURNAL_OWNER_RESULT_CAPACITY - CONTROL_RESULT_CAPACITY;
const CONTROL_RECEIVE_POLL: Duration = Duration::from_millis(50);

impl InputClass {
    fn result_limit(self) -> usize {
        match self {
            Self::Ordinary => ORDINARY_RESULT_CAPACITY,
            Self::Control => CONTROL_RESULT_CAPACITY,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResultBudget {
    remaining: [usize; 2],
    unavailable: bool,
}
impl ResultBudget {
    fn new() -> Self {
        Self {
            remaining: [ORDINARY_RESULT_CAPACITY, CONTROL_RESULT_CAPACITY],
            unavailable: false,
        }
    }
    fn valid(&self) -> bool {
        !self.unavailable
            && self.remaining[0] <= ORDINARY_RESULT_CAPACITY
            && self.remaining[1] <= CONTROL_RESULT_CAPACITY
    }
}

#[derive(Clone)]
struct OwnerCommandSenders {
    ordinary: TrackedCommandSender,
    control: TrackedCommandSender,
}
impl OwnerCommandSenders {
    fn sender(&self, class: InputClass) -> &TrackedCommandSender {
        match class {
            InputClass::Ordinary => &self.ordinary,
            InputClass::Control => &self.control,
        }
    }
}

#[derive(Clone)]
struct TrackedCommandSender {
    sender: SyncSender<QueuedJournalCommand>,
    metrics: Arc<QueueMetrics>,
    class: InputClass,
}
impl TrackedCommandSender {
    fn try_send(&self, command: JournalCommand) -> Result<(), TrySendError<JournalCommand>> {
        self.metrics
            .send(&self.sender, self.class, command)
            .map_err(|error| match error {
                TrySendError::Full(queued) => TrySendError::Full(queued.discard()),
                TrySendError::Disconnected(queued) => TrySendError::Disconnected(queued.discard()),
            })
    }
}
struct TrackedCommandReceiver(Receiver<QueuedJournalCommand>);
impl TrackedCommandReceiver {
    fn try_recv(&self) -> Result<JournalCommand, TryRecvError> {
        self.0.try_recv().map(QueuedJournalCommand::dispatch)
    }
    fn recv_timeout(&self, timeout: Duration) -> Result<JournalCommand, RecvTimeoutError> {
        self.0
            .recv_timeout(timeout)
            .map(QueuedJournalCommand::dispatch)
    }
}

struct QueuedJournalCommand {
    command: JournalCommand,
    ticket: QueueTicket,
}
impl QueuedJournalCommand {
    fn dispatch(self) -> JournalCommand {
        let Self {
            command,
            mut ticket,
        } = self;
        ticket.dequeued();
        command
    }
    fn discard(self) -> JournalCommand {
        let Self { command, ticket } = self;
        drop(ticket);
        command
    }
}

struct OwnerCommandReceivers {
    ordinary: TrackedCommandReceiver,
    control: TrackedCommandReceiver,
}
impl OwnerCommandReceivers {
    fn channel() -> (OwnerCommandSenders, Self) {
        let (ordinary_tx, ordinary) = mpsc::sync_channel(ORDINARY_COMMAND_CAPACITY);
        let (control_tx, control) = mpsc::sync_channel(CONTROL_COMMAND_CAPACITY);
        let metrics = Arc::new(QueueMetrics::default());
        (
            OwnerCommandSenders {
                ordinary: TrackedCommandSender {
                    sender: ordinary_tx,
                    metrics: Arc::clone(&metrics),
                    class: InputClass::Ordinary,
                },
                control: TrackedCommandSender {
                    sender: control_tx,
                    metrics,
                    class: InputClass::Control,
                },
            },
            Self {
                ordinary: TrackedCommandReceiver(ordinary),
                control: TrackedCommandReceiver(control),
            },
        )
    }

    // FIFO within each class; no global FIFO promise. A control request arriving after the
    // control check may follow one selected ordinary command. No running SQL is preempted.
    #[cfg(test)]
    fn recv(&self) -> Option<JournalCommand> {
        self.recv_with_idle(|| {})
    }
    fn recv_with_idle(&self, mut idle: impl FnMut()) -> Option<JournalCommand> {
        loop {
            idle();
            let control_disconnected = match self.control.try_recv() {
                Ok(command) => return Some(command),
                Err(TryRecvError::Disconnected) => true,
                Err(TryRecvError::Empty) => false,
            };
            let ordinary_disconnected = match self.ordinary.try_recv() {
                Ok(command) => return Some(command),
                Err(TryRecvError::Disconnected) => true,
                Err(TryRecvError::Empty) => false,
            };
            if control_disconnected && ordinary_disconnected {
                return None;
            }
            // Wait on control when connected, so controls wake an idle worker immediately.
            // Ordinary arrival is revisited within 50ms; disconnection wakes the wait too.
            let receiver = if control_disconnected {
                &self.ordinary
            } else {
                &self.control
            };
            match receiver.recv_timeout(CONTROL_RECEIVE_POLL) {
                Ok(command) => return Some(command),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn reject_pending(&self, error: impl Fn() -> OperationJournalOwnerError) {
        for receiver in [&self.control, &self.ordinary] {
            while let Ok(command) = receiver.0.try_recv() {
                reject_command(command.discard(), error());
            }
        }
    }
}

/// Aggregate input credit for accepted queued and executing commands, including a 64 KiB control margin.
pub const OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES: usize = MAX_RECORD_BYTES * 4;

/// Lifecycle state visible at the profile-owned journal boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationJournalOwnerStatus {
    /// The worker has been started but has not entered its command loop.
    Initializing,
    /// The worker owns the opened journal and admits bounded commands.
    Available,
    /// The worker stopped after a panic or another unrecoverable worker failure.
    Unavailable {
        /// Bounded reason retained for diagnostics.
        reason: String,
    },
    /// New commands are closed while accepted commands drain.
    Closing,
    /// The journal connection and owner worker have been released.
    Closed,
}

/// Failure to start the profile-owned journal worker.
#[derive(Debug, Error)]
pub enum OperationJournalOwnerStartError {
    /// The operating system rejected creation of the worker thread.
    #[error("failed to start operation journal owner thread: {0}")]
    ThreadSpawn(#[source] io::Error),
}

/// Nonblocking result of submitting one bounded journal command.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OperationJournalOwnerQueueError {
    /// The owner worker has not entered its command loop.
    #[error("operation journal owner is initializing")]
    Initializing,
    /// The private ordinary or control command queue is full.
    #[error("operation journal owner command queue is full")]
    Full,
    /// All result handles in the private ordinary or control class are outstanding.
    #[error("operation journal owner result capacity is full")]
    ResultsFull,
    /// The bounded input-byte budget is currently exhausted.
    #[error(
        "operation journal owner queued input exceeds its {limit}-byte budget (requested {requested} bytes)"
    )]
    InputBytesFull {
        /// Encoded input limit of the private ordinary or control credit class.
        limit: usize,
        /// Encoded command input bytes requested by this submission.
        requested: usize,
    },
    /// The encoded record exceeded the bounded journal record contract before queue admission.
    #[error("operation journal owner record exceeds the {maximum}-byte bound: {actual} bytes")]
    RecordTooLarge {
        /// Encoded bytes observed before admission.
        actual: usize,
        /// Maximum encoded record bytes accepted by the journal codec.
        maximum: usize,
    },
    /// The record could not pass the bounded codec before queue admission.
    #[error("operation journal owner record is invalid: {reason}")]
    InvalidRecord {
        /// Bounded codec diagnostic.
        reason: String,
    },
    /// A typed control observation failed validation before queue admission.
    #[error("operation journal owner control input is invalid: {reason}")]
    InvalidInput {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// The owner worker failed before this command could be admitted.
    #[error("operation journal owner is unavailable: {reason}")]
    Unavailable {
        /// Bounded worker diagnostic.
        reason: String,
    },
    /// The owner is closing or has already closed.
    #[error("operation journal owner is closed")]
    Closed,
}

/// Typed rejection from the common prepared-admission boundary.  These values retain only
/// bounded facts and identities; they never carry a provider lease, descriptor, or other
/// execution authority across the queue.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PreparedAdmissionError {
    /// The plan or command envelope cannot be represented by the current initial-intent codec.
    #[error("prepared admission input is unsupported: {reason}")]
    Unsupported {
        /// Bounded explanation of the unsupported input.
        reason: String,
    },
    /// Durable ordinary-claim evidence is not safe to coexist with a new intent.
    #[error("prepared admission is blocked by journal recovery: {reason}")]
    RecoveryRequired {
        /// Bounded recovery diagnostic.
        reason: String,
    },
    /// The retained recovery reserve is below the protected floor.
    #[error("prepared admission recovery reserve is low on {volume:?}: {free_bytes} bytes free")]
    RecoveryReserveLow {
        /// Volume whose protected reserve is low.
        volume: VolumeIdentity,
        /// Fresh free-space observation.
        free_bytes: u64,
    },
    /// Existing ledger pressure leaves only recovery-only free space.
    #[error(
        "prepared admission requires {required_bytes} bytes on {volume:?}, only {available_bytes} remain"
    )]
    DiskPressureRecoveryOnly {
        /// Volume whose incremental capacity is pressured.
        volume: VolumeIdentity,
        /// Checked bytes required by existing and new claims.
        required_bytes: u64,
        /// Checked bytes available above the protected floor.
        available_bytes: u64,
    },
    /// The retained descriptor or owner authority could not produce a fresh observation.
    #[error("prepared admission descriptor refresh was rejected: {reason}")]
    DescriptorRefreshRejected {
        /// Bounded stale, replaced, offline, or unsupported-authority diagnostic.
        reason: String,
    },
    /// The stable claim identity already exists in live or durable history.
    #[error("prepared admission claim identity is already retained: {claim_id}")]
    DuplicateClaim {
        /// Existing stable claim identity.
        claim_id: AdmissionClaimId,
    },
    /// The provider or store could not retain an unambiguous typed result.
    #[error("prepared admission failed: {reason}")]
    Failed {
        /// Bounded failure diagnostic.
        reason: String,
    },
}

/// Durable settlement was retained after an operation identity was allocated. Callers must keep
/// this identity and claim set for owner-thread reconciliation; it is not a pre-ID gate result.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PreparedAdmissionRetainedError {
    /// The intent committed, but provider disposition/readback acknowledgement was not complete.
    #[error("prepared admission committed but requires reconciliation for {receipt:?}: {reason}")]
    Committed {
        /// Exact durable intent receipt.
        receipt: JournalInsertReceipt,
        /// Stable claim identities bound to the retained capacity.
        claim_ids: Vec<AdmissionClaimId>,
        /// Bounded reconciliation reason.
        reason: String,
    },
    /// The journal write outcome is uncertain and the provider charge remains retained.
    #[error(
        "prepared admission persistence remains uncertain for {operation_id} at {stage:?}: {reason}"
    )]
    Uncertain {
        /// Operation identity supplied to the attempted insert.
        operation_id: OperationId,
        /// Stable claim identities retained by the lease.
        claim_ids: Vec<AdmissionClaimId>,
        /// Durable write boundary reached by the attempt.
        stage: JournalInsertWriteStage,
        /// Sequence selected before a post-insert failure, when known.
        attempted_sequence: Option<i64>,
        /// Bounded reconciliation reason.
        reason: String,
    },
}

impl PreparedAdmissionRetainedError {
    fn from_failure(failure: JournalInsertFailure, claim_ids: Vec<AdmissionClaimId>) -> Self {
        let operation_id = failure.operation_id();
        let stage = failure.stage();
        let attempted_sequence = failure.attempted_sequence();
        let reason = failure.error().to_string();
        Self::Uncertain {
            operation_id,
            claim_ids,
            stage,
            attempted_sequence,
            reason,
        }
    }

    fn from_failure_with_reason(
        failure: JournalInsertFailure,
        claim_ids: Vec<AdmissionClaimId>,
        reason: String,
    ) -> Self {
        let operation_id = failure.operation_id();
        let stage = failure.stage();
        let attempted_sequence = failure.attempted_sequence();
        Self::Uncertain {
            operation_id,
            claim_ids,
            stage,
            attempted_sequence,
            reason,
        }
    }
}

/// The owner allocated an identity, then confirmed rollback and released the provisional charge.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("prepared admission rolled back for {operation_id}: {reason}")]
pub struct PreparedAdmissionRolledBackError {
    /// Operation identity whose intent did not remain durable.
    pub operation_id: OperationId,
    /// Bounded rollback reason.
    pub reason: String,
}

impl From<CapacityProviderError> for PreparedAdmissionError {
    fn from(error: CapacityProviderError) -> Self {
        match error {
            CapacityProviderError::RecoveryReserveLow { volume, free_bytes } => {
                Self::RecoveryReserveLow { volume, free_bytes }
            }
            CapacityProviderError::DiskPressureRecoveryOnly {
                volume,
                required_bytes,
                available_bytes,
            } => Self::DiskPressureRecoveryOnly {
                volume,
                required_bytes,
                available_bytes,
            },
            CapacityProviderError::DuplicateClaim { claim_id } => Self::DuplicateClaim { claim_id },
            CapacityProviderError::FactsUnavailable { volume } => Self::DescriptorRefreshRejected {
                reason: format!("fresh descriptor facts are unavailable for {volume:?}"),
            },
            CapacityProviderError::UnsupportedFacts => Self::DescriptorRefreshRejected {
                reason: String::from("fresh descriptor facts are unsupported"),
            },
            CapacityProviderError::UnsupportedAllocationUnit { volume } => {
                Self::DescriptorRefreshRejected {
                    reason: format!(
                        "fresh descriptor allocation unit is unsupported for {volume:?}"
                    ),
                }
            }
            CapacityProviderError::Unavailable => Self::DescriptorRefreshRejected {
                reason: String::from("capacity provider is unavailable for live admission"),
            },
            CapacityProviderError::RecoveryRequired => Self::RecoveryRequired {
                reason: String::from("an earlier persistence outcome remains audit-pending"),
            },
            other => Self::Failed {
                reason: other.to_string(),
            },
        }
    }
}

/// Failure returned by an admitted journal command.
#[derive(Debug, Error)]
pub enum OperationJournalOwnerError {
    /// Original namespace failure retaining its pool backing through caller destruction.
    #[error(transparent)]
    RetainedBacking(RetainedCommandError),
    /// Original memory-only recording preparation could not be retained or revalidated.
    #[error(transparent)]
    RecordingProducerCustody(#[from] RecordingProducerCustodyError),
    /// Original terminal work could not retain its existing-pool input/reply credit.
    #[error("recording terminal input refused: {0}")]
    RecordingTerminalInput(#[source] OperationJournalOwnerQueueError),
    /// Original Recording mutation and rollback both retain uncertain outcomes.
    #[error("recording terminal rollback unconfirmed: {rollback}; original: {original}")]
    RecordingTerminalRollbackUncertain {
        /// Exact primary owner/Source/store failure.
        original: Box<OperationJournalOwnerError>,
        /// Exact rollback failure under the same original backing.
        rollback: Box<OperationJournalStoreError>,
    },
    /// Original pre-lease custody could not be installed or revalidated.
    #[error(transparent)]
    LegacyAdoptionCustody(#[from] LegacyAdoptionCustodyError),
    /// The underlying journal store rejected or could not complete the command.
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
    /// The bounded record bytes could not be decoded on the owner thread.
    #[error(transparent)]
    Codec(#[from] DecodeError),
    /// A recovery page could not be read or retained its audit-required state.
    #[error(transparent)]
    Recovery(#[from] RecoveryScanError),
    /// A source-owner authority handshake failed closed.
    #[error(transparent)]
    SourceAuthority(#[from] SourceAuthorityError),
    /// Original-owner pre-intent WAL preparation refused; no identity or intent was issued.
    #[error(transparent)]
    WalPreparation(#[from] WalPreparationError),
    /// An authenticated metadata handoff failed closed without effect permission.
    #[error(transparent)]
    SourceMetadata(#[from] crate::sample_sources::db::SourceRegistryMetadataHandoffError),
    /// A source registration or retirement message conflicts with retained owner state.
    #[error("source authority registration conflict: {reason}")]
    SourceAuthorityConflict {
        /// Bounded conflict diagnostic.
        reason: String,
    },
    /// A source registration retained bounded owner-side state after a partial or uncertain
    /// physical binding. The source must drain or reconcile this identity before reuse.
    #[error("source authority registration requires recovery: {reason}")]
    SourceAuthorityRecoveryRequired {
        /// Bounded stage and failure diagnostic.
        reason: String,
        /// Receipt retained when physical binding began before the failure. `None` means the
        /// request was rejected before the owner installed any registration state.
        receipt: Option<SourceAuthorityRegistrationReceipt>,
    },
    /// The common prepared-admission gate rejected before durable intent allocation.
    #[error(transparent)]
    PreparedAdmission(#[from] PreparedAdmissionError),
    /// The owner allocated an identity but retained an uncertain or committed settlement.
    #[error(transparent)]
    PreparedAdmissionRetained(#[from] PreparedAdmissionRetainedError),
    /// The owner allocated an identity and confirmed rollback before returning the failure.
    #[error(transparent)]
    PreparedAdmissionRolledBack(#[from] PreparedAdmissionRolledBackError),
    /// A post-intent chunk claim or permit transition failed closed. Durable claim/release
    /// evidence remains authoritative and must be retried or reconciled by its exact identity.
    #[error("operation journal chunk claim failed: {reason}")]
    ChunkClaim {
        /// Bounded chunk transition diagnostic.
        reason: String,
    },
    /// Live source, checkpoint, or one-shot publication-grant validation failed closed.
    #[error("operation journal staging grant failed: {reason}")]
    StagingGrant {
        /// Bounded staging diagnostic.
        reason: String,
    },
    /// Publication already entered or remains unsettled; cancellation was not recorded.
    #[error("operation {operation_id:?} cancellation awaits publication reconciliation")]
    PublicationCancellationPending {
        /// Original operation whose publication custody prevents cancellation.
        operation_id: OperationId,
    },
    /// Original publication custody prevents releasing this operation's chunk charges.
    #[error("operation {operation_id:?} chunk release awaits publication reconciliation")]
    PublicationChargeReleasePending {
        /// Operation whose entered, unsettled or unknown publication retains its charges.
        operation_id: OperationId,
    },
    /// Publication custody prevents changing this operation's progress or retry state.
    #[error("operation {operation_id:?} mutation awaits publication reconciliation")]
    PublicationMutationPending {
        /// Original operation whose publication attempt remains entered or unresolved.
        operation_id: OperationId,
    },
    /// Source retirement cannot invalidate an entered or unresolved publication binding.
    #[error("source retirement awaits publication reconciliation for {operation_id:?}")]
    PublicationSourceRetirementPending {
        /// Original operation retaining this source registration.
        operation_id: OperationId,
    },
    /// An internally serialized control input could not be decoded on the owner thread.
    #[error("operation journal owner control input could not be decoded: {reason}")]
    InvalidInput {
        /// Bounded decoder diagnostic.
        reason: String,
    },
    /// Full-operation admission remains fail-closed until the capacity provider is integrated.
    #[error("full-operation admission is unavailable until the capacity provider is integrated")]
    CapacityProviderUnavailable,
    /// The worker panicked while executing the command.
    #[error("operation journal owner worker panicked")]
    WorkerPanicked,
    /// The result channel disconnected before a completion was delivered.
    #[error("operation journal owner worker disconnected before completion")]
    WorkerDisconnected,
    /// The owner was closed before this command could complete.
    #[error("operation journal owner is closed")]
    Closed,
}

/// Nonblocking polling result for an admitted journal request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum OperationJournalRequestError {
    /// The command has not produced a result yet.
    #[error("operation journal request result is not ready")]
    Empty,
    /// The owner stopped before a result could be read.
    #[error("operation journal request result channel disconnected")]
    Disconnected,
}

/// A bounded, clone-free result handle for one admitted journal command.
///
/// The result permit remains held while the command is queued and after its result is produced.
/// It is released when the result is consumed or when this handle is dropped. Dropping the handle
/// does not cancel the admitted command; the worker still executes it and simply discards a result
/// that has no receiver.
pub struct OperationJournalRequest<T> {
    receiver: Option<Receiver<Result<T, OperationJournalOwnerError>>>,
    historical_reply: Option<HistoricalReply>,
    result_permit: Option<ResultPermit>,
}

impl<T> OperationJournalRequest<T> {
    /// Poll the result without waiting for the worker.
    pub fn try_result(
        &mut self,
    ) -> Result<Result<T, OperationJournalOwnerError>, OperationJournalRequestError> {
        match self
            .receiver
            .as_ref()
            .ok_or(OperationJournalRequestError::Disconnected)?
            .try_recv()
        {
            Ok(result) => {
                self.result_permit.take();
                Ok(namespace_credit::retain_result_error(
                    result,
                    self.historical_reply.take(),
                ))
            }
            Err(TryRecvError::Empty) => Err(OperationJournalRequestError::Empty),
            Err(TryRecvError::Disconnected) => {
                self.result_permit.take();
                self.historical_reply.take();
                Err(OperationJournalRequestError::Disconnected)
            }
        }
    }

    /// Wait up to `timeout` for the result without changing command admission semantics.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Result<T, OperationJournalOwnerError>, OperationJournalRequestError> {
        match self
            .receiver
            .as_ref()
            .ok_or(OperationJournalRequestError::Disconnected)?
            .recv_timeout(timeout)
        {
            Ok(result) => {
                self.result_permit.take();
                Ok(namespace_credit::retain_result_error(
                    result,
                    self.historical_reply.take(),
                ))
            }
            Err(RecvTimeoutError::Timeout) => Err(OperationJournalRequestError::Empty),
            Err(RecvTimeoutError::Disconnected) => {
                self.result_permit.take();
                self.historical_reply.take();
                Err(OperationJournalRequestError::Disconnected)
            }
        }
    }
}

impl<T> Drop for OperationJournalRequest<T> {
    fn drop(&mut self) {
        // Destroy unconsumed channel contents before releasing their count/backing.
        drop(self.receiver.take());
        self.result_permit.take();
        self.historical_reply.take();
    }
}

/// Cloneable access to one profile-owned operation-journal worker.
///
/// Clones retain only shared bounded admission state. They do not retain the profile guard, the
/// SQLite store, or a worker join handle. The [`OperationJournalOwner`] remains responsible for
/// lifecycle shutdown and drain.
#[derive(Clone)]
pub struct OperationJournalClient {
    shared: Arc<OwnerShared>,
}

/// A private, authority-free initial-admission template.  It is intentionally separate from
/// `OperationId`: the owner allocates that identity only after this template, fresh descriptor
/// facts, current recovery evidence, and the one provisional lease have all passed validation.
pub(crate) struct PreparedAdmissionInput {
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    args: UniversalCommandArgs,
    participants: ParticipantSet,
    plan: FullCapacityPlan,
    claims: AdmissionClaimInventory,
    timestamp_unix_ms: i64,
    #[cfg(all(test, target_os = "macos"))]
    facts_fault: Option<PreparedAdmissionFactsFault>,
}

impl PreparedAdmissionInput {
    fn validate_shape(&self) -> Result<(), PreparedAdmissionError> {
        if self.timestamp_unix_ms < 0 {
            return Err(PreparedAdmissionError::Unsupported {
                reason: String::from("prepared admission timestamp must be non-negative"),
            });
        }
        self.participants
            .validate_for_new_intent()
            .map_err(|error| PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            })?;
        self.args
            .validate_against_participants(&self.participants)
            .map_err(|error| PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            })?;
        derive_work_plan(&self.args, &self.participants).map_err(|error| {
            PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            }
        })?;
        if self.universal_intent.participants() != &self.participants {
            return Err(PreparedAdmissionError::Unsupported {
                reason: String::from(
                    "prepared admission universal intent participants do not match its template",
                ),
            });
        }
        self.universal_intent
            .validate_for_new_intent(
                self.validation_operation_id(),
                self.intent.actor,
                &self.intent.kind,
            )
            .map_err(|error| PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            })?;
        if self.universal_intent.command_kind() != self.args.command_kind() {
            return Err(PreparedAdmissionError::Unsupported {
                reason: String::from(
                    "prepared admission universal intent command does not match its arguments",
                ),
            });
        }
        if matches!(&self.args, UniversalCommandArgs::SourceLegacyAdoption(_)) {
            return Err(PreparedAdmissionError::Unsupported {
                reason: "legacy adoption requires the original-owner capacity and durable admission bridge".into(),
            });
        }
        if matches!(
            &self.args,
            UniversalCommandArgs::SourceDerivedLayout(_)
                | UniversalCommandArgs::DecodedEdit(_)
                | UniversalCommandArgs::DecodedExport(_)
                | UniversalCommandArgs::DecodedExportBatch(_)
        ) {
            return Err(PreparedAdmissionError::Unsupported {
                reason: String::from(
                    "full-progress-only command arguments have no initial-intent codec",
                ),
            });
        }
        self.claims.validate_against(&self.plan).map_err(|error| {
            PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            }
        })?;
        if self.plan.allocations().iter().any(|allocation| {
            allocation.logical_bytes() > i64::MAX as u64
                || allocation.allocated_bytes() > i64::MAX as u64
                || allocation.protected_free_bytes() > i64::MAX as u64
        }) || self
            .claims
            .claims()
            .iter()
            .any(|claim| claim.allocated_bytes() > i64::MAX as u64)
        {
            return Err(PreparedAdmissionError::Unsupported {
                reason: String::from("prepared admission exceeds SQLite integer bounds"),
            });
        }
        let validation_record =
            prepared_record(self.validation_operation_id(), self).map_err(|error| {
                PreparedAdmissionError::Unsupported {
                    reason: error.to_string(),
                }
            })?;
        OperationJournalStore::validate_initial_intent_record(&validation_record).map_err(
            |error| PreparedAdmissionError::Unsupported {
                reason: error.to_string(),
            },
        )?;
        Ok(())
    }

    fn validation_operation_id(&self) -> OperationId {
        if let UniversalCommandArgs::Recording(proposal) = &self.args {
            return proposal.operation_id();
        }
        if let UniversalCommandArgs::SourceRegistryRetirement(args) = &self.args {
            return args
                .operation_id()
                .expect("validated frozen retirement operation");
        }
        let parent = self.universal_intent.parent_operation_id();
        [
            uuid::Uuid::nil(),
            uuid::Uuid::from_u128(u128::MAX),
            uuid::Uuid::from_u128(1),
        ]
        .into_iter()
        .find(|candidate| parent.is_none_or(|parent| parent.as_uuid() != *candidate))
        .map(OperationId::from_uuid)
        .expect("one validation operation identity must differ from one optional parent")
    }

    /// Build one validated common-admission template for a future owner adapter. The fields stay
    /// private so callers cannot forge a partially checked queued command or carry authority in
    /// the request; the owner repeats the validation at execution time.
    #[allow(dead_code)]
    pub(crate) fn try_new(
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        args: UniversalCommandArgs,
        participants: ParticipantSet,
        plan: FullCapacityPlan,
        claims: AdmissionClaimInventory,
        timestamp_unix_ms: i64,
    ) -> Result<Self, PreparedAdmissionError> {
        let prepared = Self {
            intent,
            universal_intent,
            args,
            participants,
            plan,
            claims,
            timestamp_unix_ms,
            #[cfg(all(test, target_os = "macos"))]
            facts_fault: None,
        };
        prepared.validate_shape()?;
        Ok(prepared)
    }

    #[cfg(test)]
    fn for_test(
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        args: UniversalCommandArgs,
        participants: ParticipantSet,
        plan: FullCapacityPlan,
        claims: AdmissionClaimInventory,
    ) -> Self {
        Self {
            intent,
            universal_intent,
            args,
            participants,
            plan,
            claims,
            timestamp_unix_ms: 1,
            #[cfg(all(test, target_os = "macos"))]
            facts_fault: None,
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    fn with_facts_fault(mut self, fault: PreparedAdmissionFactsFault) -> Self {
        self.facts_fault = Some(fault);
        self
    }
}

/// The single asynchronous owner of one already-open operation journal.
///
/// The supplied store must have passed the caller's recovery gate before handoff. The owner takes
/// the store by value and releases it on its worker thread after accepted commands have drained.
pub struct OperationJournalOwner {
    shared: Arc<OwnerShared>,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    prepared_admission_id_allocations: Arc<AtomicUsize>,
    #[cfg(all(test, unix))]
    synthetic_chunk_write_invocations: Arc<AtomicUsize>,
}

/// Worker-owned runtime state with an explicit teardown order.
///
/// The profile guard clone is deliberately kept separate from the store's clone. This ensures
/// that the journal connection and its root lease close before the shared volume lock, while the
/// final profile guard remains alive until both have been released. Every fallible bootstrap path
/// and worker panic unwinds through this same guard.
#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OwnerRuntimeDropObservation {
    profile_held_after_store_drop: bool,
    volume_held_after_store_drop: bool,
    profile_available_after_teardown: bool,
    volume_available_after_teardown: bool,
}

#[cfg(test)]
struct OwnerRuntimeDropProbe {
    descriptor: std::fs::File,
    registry_path: std::path::PathBuf,
    volume: crate::durable_capacity::VolumeIdentity,
    profile_identity: String,
}

#[cfg(test)]
type OwnerRuntimeDropObserver = Arc<Mutex<Option<OwnerRuntimeDropObservation>>>;

#[cfg(test)]
impl OwnerRuntimeDropProbe {
    fn profile_is_held() -> bool {
        match WritableProfileGuard::acquire_current() {
            Ok(guard) => {
                drop(guard);
                false
            }
            Err(_) => true,
        }
    }

    fn volume_is_held(&self) -> bool {
        match super::capacity_volume_owner::acquire_fixture_for_test(
            &self.descriptor,
            &self.registry_path,
            self.volume.clone(),
            &self.profile_identity,
        ) {
            Ok(capability) => {
                drop(capability);
                false
            }
            Err(_) => true,
        }
    }

    fn observe(&self) -> OwnerRuntimeDropObservation {
        let profile_held =
            panic::catch_unwind(AssertUnwindSafe(Self::profile_is_held)).unwrap_or(false);
        let volume_held =
            panic::catch_unwind(AssertUnwindSafe(|| self.volume_is_held())).unwrap_or(false);
        OwnerRuntimeDropObservation {
            profile_held_after_store_drop: profile_held,
            volume_held_after_store_drop: volume_held,
            profile_available_after_teardown: !profile_held,
            volume_available_after_teardown: !volume_held,
        }
    }
}

#[cfg(test)]
fn record_owner_runtime_drop_observation(
    observer: &OwnerRuntimeDropObserver,
    observation: OwnerRuntimeDropObservation,
) {
    if let Ok(mut slot) = observer.lock() {
        if let Some(existing) = slot.as_mut() {
            existing.profile_held_after_store_drop |= observation.profile_held_after_store_drop;
            existing.volume_held_after_store_drop |= observation.volume_held_after_store_drop;
            existing.profile_available_after_teardown |=
                observation.profile_available_after_teardown;
            existing.volume_available_after_teardown |= observation.volume_available_after_teardown;
        } else {
            *slot = Some(observation);
        }
    }
}

struct SourceVolumeBinding {
    _current_reserve_failure: super::store::LegacyReserveFailureSink,
    #[allow(dead_code)]
    reserve: Option<OwnedReserveAllocation>,
    #[allow(dead_code)]
    owner: Option<VolumeOwnershipCapability>,
    _current_volume_failure: super::capacity_volume_owner::LegacyVolumeFailureBacking,
    #[cfg(all(test, unix))]
    _current_before_root_drop: Option<current_source_custody_tests::BeforeRootDrop>,
    // Original selected source ancestor closes after all native children and ownership backing.
    _current_root: Option<crate::sample_sources::db::BoundDatabaseRoot>,
    #[cfg(all(test, unix))]
    _current_after_root_drop: Option<current_source_custody_tests::AfterRootDrop>,
    allocation_unit: Option<u64>,
    readiness: SourceVolumeBindingReadiness,
    registrations: BTreeSet<crate::sample_sources::db::SourceRegistrationId>,
    _current_input: Option<current_source_metadata::CurrentInput>,
}

/// Owner-thread identity for one durable Issued chunk. The permit returned to a caller is not a
/// write authority by itself; this live registry is required at every bounded entry/release.
struct IssuedChunkTicket {
    unused_release: Option<Uuid>,
    chunk_id: ChunkId,
    operation_id: OperationId,
    identity: ChunkWriteIdentity,
    live_output: Option<OutputIdentity>,
    live_target: Option<RootRegistrationRef>,
    live_source_fences: Vec<SourceFence>,
    _credit: Option<StartupCredit>,
}

/// Proof retained after an unused issuance has been consumed by a release attempt. It forbids
/// entry while allowing the exact durable release to be retried after rollback uncertainty or a
/// provider reconciliation failure.
struct PendingChunkRelease {
    chunk_id: ChunkId,
    operation_id: OperationId,
    issue_id: Uuid,
    release_id: Uuid,
}

/// Owner-issued opaque ticket for one durable chunk issuance. Only read-only identity getters
/// cross the queue boundary; entry construction and consumption stay private to this module.
#[derive(Debug)]
pub(crate) struct ChunkWritePermit {
    chunk_id: ChunkId,
    operation_id: OperationId,
    issue_id: Uuid,
    identity: ChunkWriteIdentity,
    logical_length: u64,
    allocated_bytes: u64,
    _credit: Option<StartupCredit>,
}

/// Authority-free effect identity supplied before the journal issues a chunk.
/// The output UUID is bound separately from the claim's arbitrary string output key.
#[allow(dead_code)] // Consumed by the file-owner adapter in the next OPT-1857 stage.
pub(crate) struct LiveChunkEffectSpec {
    effect_id: EffectId,
    output: OutputIdentity,
    target: RootRegistrationRef,
    source_fences: Vec<SourceFence>,
    offset: u64,
    logical_bytes: u64,
    payload_bytes: u64,
    payload_hash: [u8; 32],
}

fn recording_effect_matches(
    proposal: &RecordingProposal,
    captured: &RecordingCapturedChunk,
    spec: &LiveChunkEffectSpec,
) -> bool {
    proposal.matches_staging_root(spec.target)
        && captured.output_id() == spec.output.as_uuid()
        && captured.offset() == spec.offset
        && captured.bytes().len() as u64 == spec.logical_bytes
        && captured.bytes().len() as u64 == spec.payload_bytes
        && *blake3::hash(captured.bytes()).as_bytes() == spec.payload_hash
}

impl LiveChunkEffectSpec {
    fn queued_bytes(&self) -> Option<usize> {
        let mut bytes = CONTROL_INPUT_BYTES.checked_add(
            self.source_fences
                .capacity()
                .checked_mul(std::mem::size_of::<SourceFence>())?,
        )?;
        for fence in &self.source_fences {
            bytes = bytes.checked_add(fence.dynamic_backing_bytes()?)?;
        }
        Some(bytes)
    }

    #[allow(dead_code)] // Consumed by the file-owner adapter in the next OPT-1857 stage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        effect_id: EffectId,
        output: OutputIdentity,
        target: RootRegistrationRef,
        source_fences: Vec<SourceFence>,
        offset: u64,
        logical_bytes: u64,
        payload: &[u8],
    ) -> Result<Self, OperationJournalOwnerQueueError> {
        if effect_id.as_uuid().is_nil()
            || output.as_uuid().is_nil()
            || logical_bytes == 0
            || logical_bytes > MAX_RECORD_BYTES as u64
            || payload.is_empty()
            || payload.len() as u64 > logical_bytes
            || source_fences.len() != 1
            || offset.checked_add(logical_bytes).is_none()
        {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("live chunk effect has invalid bounds or source fences"),
            });
        }
        for fence in &source_fences {
            fence
                .validate()
                .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
                    reason: error.to_string(),
                })?;
        }
        Ok(Self {
            effect_id,
            output,
            target,
            source_fences,
            offset,
            logical_bytes,
            payload_bytes: payload.len() as u64,
            payload_hash: *blake3::hash(payload).as_bytes(),
        })
    }
}

/// One journal-issued live chunk grant. Only the issuing worker can construct it; the retained
/// source pin and non-Clone chunk permit are never reconstructed from a row or a UUID.
#[allow(dead_code)] // Consumed by the file-owner adapter in the next OPT-1857 stage.
pub(crate) struct LiveChunkEffectGrant {
    permit: ChunkWritePermit,
    source_pin: SourceEffectPin,
    spec: LiveChunkEffectSpec,
    output_key: String,
}

/// A single publication attempt backed by an acknowledged CopyValidated checkpoint.
/// The private live source pin and file-owner result are consumed together; retained journal
/// bytes and a replayed checkpoint cannot construct this value.
#[allow(dead_code)] // The publication effect itself belongs to the subsequent route.
pub(crate) struct StagingPublicationGrant {
    target: PreparedPublicationTarget,
    completion_fingerprint: [u8; 32],
    fence: publication_fence::PendingFence,
    charge: super::capacity_provider::PublicationChargeBinding,
    native: native_read_capsule::NativeReadCapsule,
    source_pin: SourceEffectPin,
    resource_admission: Arc<ResourceAdmission>,
    input: InputPermit,
}

impl std::fmt::Debug for StagingPublicationGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StagingPublicationGrant")
            .field("operation_id", &self.target.validation().operation_id())
            .field("effect_id", &self.target.validation().effect_id())
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)] // Consumed by the publication adapter when that route is installed.
impl StagingPublicationGrant {
    pub(crate) fn original_target(&self) -> &PreparedPublicationTarget {
        &self.target
    }

    pub(crate) fn original_worker_started(&self) {
        self.fence.original_worker_started();
    }
    pub(crate) fn retain_worker_completion(
        &self,
        result: &crate::file_operation_owner::FileOperationResult,
        worker: crate::file_operation_owner::OwnerIdentity,
    ) {
        let (digest, exact) = crate::file_operation_owner::completion_observation(
            &self.target,
            self.completion_fingerprint,
            result,
            worker,
        );
        let (kind, reason) =
            crate::file_operation_owner::publication_no_effect_detail(result).unwrap_or((0, ""));
        self.fence
            .complete_before_effect_detail(digest, exact, kind, reason);
    }

    pub(crate) fn begin_publication(
        self,
    ) -> Result<publication_custody::PublicationSourceUse, OperationJournalOwnerError> {
        publication_custody::begin_source_entry(
            &self.resource_admission,
            self.source_pin,
            self.target,
            self.input,
            self.fence,
            self.charge,
            self.native,
        )
    }
}

fn validate_publication_target(
    target: &PreparedPublicationTarget,
) -> Result<(), OperationJournalOwnerError> {
    if target.is_current() {
        Ok(())
    } else {
        Err(OperationJournalOwnerError::StagingGrant {
            reason: String::from("original publication target is no longer current"),
        })
    }
}

impl std::fmt::Debug for LiveChunkEffectGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveChunkEffectGrant")
            .field("chunk_id", &self.permit.chunk_id)
            .field("operation_id", &self.permit.operation_id)
            .field("issue_id", &self.permit.issue_id)
            .field("effect_id", &self.spec.effect_id)
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)] // Private file-owner adapter is not wired to an ordinary operation family.
impl LiveChunkEffectGrant {
    pub(crate) fn chunk_id(&self) -> Uuid {
        self.permit.chunk_id().as_uuid()
    }

    /// Scalar selection only; the original actor must resolve and bind its actual durable charge.
    pub(crate) fn publication_charge_selector(&self) -> publication_charge::Selector {
        publication_charge::Selector::Chunk(self.permit.chunk_id())
    }

    pub(crate) fn issue_id(&self) -> Uuid {
        self.permit.issue_id()
    }

    pub(crate) fn output(&self) -> OutputIdentity {
        self.spec.output
    }

    pub(crate) fn offset(&self) -> u64 {
        self.spec.offset
    }

    pub(crate) fn logical_bytes(&self) -> u64 {
        self.spec.logical_bytes
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.permit.operation_id()
    }

    pub(crate) fn participant_id(&self) -> super::participants::ParticipantId {
        self.permit.identity().participant_id()
    }

    pub(crate) fn effect_id(&self) -> EffectId {
        self.spec.effect_id
    }

    pub(crate) fn target(&self) -> RootRegistrationRef {
        self.spec.target
    }

    pub(crate) fn source_fences(&self) -> &[SourceFence] {
        &self.spec.source_fences
    }

    pub(crate) fn matches_command(&self, command: &FileOperationCommand) -> bool {
        if command.operation_id() != self.permit.operation_id()
            || command.participant_id() != self.permit.identity().participant_id()
            || command.effect_id() != self.spec.effect_id
            || command.target_registration() != self.spec.target
            || command.source_fences() != self.spec.source_fences
        {
            return false;
        }
        let EffectRequest::WriteStagingChunk {
            chunk_id,
            issue_id,
            output,
            offset,
            length,
            payload,
            ..
        } = command.effect()
        else {
            return false;
        };
        chunk_id.as_uuid() == self.permit.chunk_id().as_uuid()
            && issue_id.as_uuid() == self.permit.issue_id()
            && *output == self.spec.output
            && offset.get() == self.spec.offset
            && length.get() == self.spec.logical_bytes
            && payload.as_slice().len() as u64 == self.spec.payload_bytes
            && *blake3::hash(payload.as_slice()).as_bytes() == self.spec.payload_hash
    }

    pub(crate) fn begin_source_effect(&self) -> Result<SourceEffectPermit, SourceAuthorityError> {
        self.source_pin.begin_effect()
    }

    pub(crate) fn queued_backing_bytes(&self) -> Option<usize> {
        self.spec
            .queued_bytes()?
            .checked_add(self.output_key.capacity())
    }

    pub(crate) fn into_unused_permit(self) -> ChunkWritePermit {
        self.permit
    }
}

#[derive(Debug)]
struct ChunkWriteEntry {
    chunk_id: ChunkId,
    operation_id: OperationId,
    issue_id: Uuid,
    identity: ChunkWriteIdentity,
    logical_length: u64,
    allocated_bytes: u64,
    _credit: Option<StartupCredit>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
enum ChunkPermitError {
    #[error("chunk write identity does not match the issued permit")]
    IdentityMismatch,
    #[error("chunk write budget {requested} exceeds the permit allowance {allowed}")]
    BudgetExceeded { requested: u64, allowed: u64 },
}

impl ChunkWritePermit {
    #[cfg(test)]
    fn from_issue_receipt(receipt: ChunkIssueReceipt) -> Self {
        Self::from_issue_receipt_funded(receipt, None)
    }

    fn from_issue_receipt_funded(
        receipt: ChunkIssueReceipt,
        credit: Option<StartupCredit>,
    ) -> Self {
        Self {
            chunk_id: receipt.chunk_id(),
            operation_id: receipt.operation_id(),
            issue_id: receipt.issue_id(),
            identity: receipt.write_identity(),
            logical_length: receipt.logical_length(),
            allocated_bytes: receipt.allocated_bytes(),
            _credit: credit,
        }
    }

    pub(crate) fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn issue_id(&self) -> Uuid {
        self.issue_id
    }

    pub(crate) fn identity(&self) -> &ChunkWriteIdentity {
        &self.identity
    }

    pub(crate) fn logical_length(&self) -> u64 {
        self.logical_length
    }

    fn consume(
        self,
        identity: &ChunkWriteIdentity,
        logical_bytes: u64,
    ) -> Result<ChunkWriteEntry, ChunkPermitError> {
        if self.identity != *identity {
            return Err(ChunkPermitError::IdentityMismatch);
        }
        if logical_bytes == 0 || logical_bytes > self.logical_length {
            return Err(ChunkPermitError::BudgetExceeded {
                requested: logical_bytes,
                allowed: self.logical_length,
            });
        }
        Ok(ChunkWriteEntry {
            chunk_id: self.chunk_id,
            operation_id: self.operation_id,
            issue_id: self.issue_id,
            identity: self.identity,
            logical_length: self.logical_length,
            allocated_bytes: self.allocated_bytes,
            _credit: self._credit,
        })
    }
}

impl ChunkWriteEntry {
    fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    fn issue_id(&self) -> Uuid {
        self.issue_id
    }

    fn identity(&self) -> &ChunkWriteIdentity {
        &self.identity
    }

    fn logical_length(&self) -> u64 {
        self.logical_length
    }

    fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }
}

/// Owner-minted proof for releasing an unused Issued ticket. Its fields bind the release
/// transition and are visible to the store only through read-only accessors.
pub(crate) struct ChunkUnusedReleaseAuthorization {
    chunk_id: ChunkId,
    operation_id: OperationId,
    issue_id: Uuid,
    release_id: Uuid,
}

impl ChunkUnusedReleaseAuthorization {
    fn new(chunk_id: ChunkId, operation_id: OperationId, issue_id: Uuid, release_id: Uuid) -> Self {
        Self {
            chunk_id,
            operation_id,
            issue_id,
            release_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        chunk_id: ChunkId,
        operation_id: OperationId,
        issue_id: Uuid,
        release_id: Uuid,
    ) -> Self {
        Self::new(chunk_id, operation_id, issue_id, release_id)
    }

    pub(crate) fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn issue_id(&self) -> Uuid {
        self.issue_id
    }

    pub(crate) fn release_id(&self) -> Uuid {
        self.release_id
    }
}

/// A transient, owner-thread observation of descriptor-derived capacity facts.
///
/// The provenance binds every fact to the retained volume lock, profile identity, reserve root,
/// and reserve descriptor that were revalidated for this observation.  It is intentionally not
/// cached in [`CapacityProvider`], serialized, or accepted as ordinary-admission authority; a
/// later capacity transition must obtain a new observation while the retained handles are live.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveDescriptorCapacityObservation {
    facts: BTreeMap<VolumeIdentity, VolumeFacts>,
    provenance: BTreeMap<VolumeIdentity, LiveDescriptorCapacityProvenance>,
}

/// A one-shot owner-created witness for one live descriptor observation. The retained authority
/// references keep the profile, volume locks, and reserve descriptors live for the synchronous
/// provider call. The witness is deliberately non-Clone and is never queued or persisted.
pub(crate) struct LiveAdmissionWitness<'a> {
    observation: LiveDescriptorCapacityObservation,
    _profile_guard: &'a WritableProfileGuard,
    _volume_owners: Vec<&'a VolumeOwnershipCapability>,
    _reserves: Vec<&'a OwnedReserveAllocation>,
}

impl LiveAdmissionWitness<'_> {
    pub(super) fn facts(&self) -> &BTreeMap<VolumeIdentity, VolumeFacts> {
        &self.observation.facts
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveDescriptorCapacityProvenance {
    profile_identity: String,
    owner_generation: u64,
    reserve_root_identity: String,
    reserve_file_identity: String,
    allocation_unit: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceVolumeBindingReadiness {
    Incomplete,
    Complete,
}

struct SourceRegistryWriterBinding {
    source_id: String,
    lifecycle: u64,
    writer: std::sync::Weak<()>,
}

struct SourceAuthorityRecord {
    metadata_registration:
        crate::sample_sources::db::source_registry_handoff::MetadataRegistrationCustody,
    metadata_writer: Option<SourceRegistryWriterBinding>,
    request: Arc<SourceAuthorityRequest>,
    receipt: SourceAuthorityRegistrationReceipt,
    drain_marker: Option<SourceAuthorityDrainMarker>,
    readiness: SourceRegistrationReadiness,
    _current_input: Option<current_source_metadata::CurrentInput>,
}

struct RetiredSourceAuthorityRecord {
    media_root_identity: String,
    database_root_identity: String,
    database_file_identity: String,
    lease_identity: crate::sample_sources::db::SourceLeaseIdentity,
    receipt: SourceAuthorityRegistrationReceipt,
    _current_input: Option<current_source_metadata::CurrentInput>,
}

impl RetiredSourceAuthorityRecord {
    fn matches_request(&self, request: &SourceAuthorityRequest) -> bool {
        self.receipt.registration_id == request.registration_id()
            && &self.lease_identity == request.lease_identity()
            && self.media_root_identity == request.media_root().identity()
            && self.database_root_identity == request.database_root().identity()
            && self.database_file_identity == request.database_file_identity()
    }
}

#[derive(Debug)]
enum SourceRegistrationReadiness {
    Registering,
    Registered,
    Draining,
    RecoveryRequired { stage: &'static str, reason: String },
}

fn observe_retained_descriptor_inner(
    volume_owner: &VolumeOwnershipCapability,
    reserve: &mut OwnedReserveAllocation,
    profile_guard: &WritableProfileGuard,
    require_full_floor: bool,
    #[cfg(test)] facts_reader: Option<
        &dyn Fn(&mut OwnedReserveAllocation, &WritableProfileGuard) -> Result<VolumeFacts, String>,
    >,
) -> Result<
    (
        VolumeIdentity,
        VolumeFacts,
        LiveDescriptorCapacityProvenance,
    ),
    String,
> {
    let profile_identity = profile_guard.profile_root_identity().to_owned();
    if volume_owner.owner_profile_identity() != profile_identity {
        return Err(String::from(
            "retained volume owner belongs to a different profile",
        ));
    }
    let expected_volume = volume_owner.volume().clone();
    let observation = reserve.observation();
    if observation.volume() != &expected_volume {
        return Err(String::from(
            "retained reserve descriptor belongs to a different volume",
        ));
    }
    let provenance = LiveDescriptorCapacityProvenance {
        profile_identity,
        owner_generation: volume_owner.owner_generation(),
        reserve_root_identity: observation.root_identity().to_owned(),
        reserve_file_identity: observation.file_identity().to_owned(),
        allocation_unit: observation.allocation_unit(),
    };
    volume_owner
        .revalidate()
        .map_err(|error| format!("retained volume owner is no longer current: {error}"))?;
    if require_full_floor {
        let proof = reserve
            .view(profile_guard)
            .revalidate_and_prove()
            .map_err(|error| format!("retained reserve full proof is unavailable: {error}"))?;
        if proof.volume() != &expected_volume
            || !proof.non_sparse()
            || proof.physical_bytes() < PROTECTED_FREE_SPACE_FLOOR
        {
            return Err(format!(
                "retained reserve does not meet the full protected physical floor: {} bytes",
                proof.physical_bytes()
            ));
        }
    }
    #[cfg(test)]
    let facts = match facts_reader {
        Some(reader) => reader(reserve, profile_guard),
        None => reserve
            .view(profile_guard)
            .fresh_capacity_facts()
            .map_err(|error| error.to_string()),
    }
    .map_err(|error| format!("retained reserve facts are unavailable: {error}"))?;
    #[cfg(not(test))]
    let facts = reserve
        .view(profile_guard)
        .fresh_capacity_facts()
        .map_err(|error| format!("retained reserve facts are unavailable: {error}"))?;
    volume_owner
        .revalidate()
        .map_err(|error| format!("retained volume owner changed during observation: {error}"))?;
    if facts.identity != expected_volume {
        return Err(String::from(
            "fresh descriptor facts differ from the retained owner volume",
        ));
    }
    if facts.allocation_unit != provenance.allocation_unit {
        return Err(String::from(
            "fresh descriptor allocation unit differs from the retained reserve",
        ));
    }
    Ok((expected_volume, facts, provenance))
}

#[cfg(all(test, target_os = "macos"))]
fn prepared_admission_fault_facts(
    fault: PreparedAdmissionFactsFault,
    reserve: &mut OwnedReserveAllocation,
    profile_guard: &WritableProfileGuard,
) -> Result<VolumeFacts, String> {
    match fault {
        PreparedAdmissionFactsFault::Unsupported => Err(String::from(
            "descriptor backend is unsupported for admission",
        )),
        PreparedAdmissionFactsFault::Replaced => Err(String::from(
            "retained reserve facts refer to a replaced descriptor",
        )),
        PreparedAdmissionFactsFault::LowReserve
        | PreparedAdmissionFactsFault::DiskPressure
        | PreparedAdmissionFactsFault::DiskPressureBoundary
        | PreparedAdmissionFactsFault::ExactBoundary => {
            let mut facts = reserve
                .view(profile_guard)
                .fresh_capacity_facts()
                .map_err(|error| error.to_string())?;
            facts.free_bytes = match fault {
                PreparedAdmissionFactsFault::LowReserve => PROTECTED_FREE_SPACE_FLOOR,
                PreparedAdmissionFactsFault::DiskPressure => {
                    PROTECTED_FREE_SPACE_FLOOR.saturating_add(1)
                }
                PreparedAdmissionFactsFault::DiskPressureBoundary => PROTECTED_FREE_SPACE_FLOOR
                    .checked_add(facts.allocation_unit.saturating_sub(1))
                    .ok_or_else(|| String::from("prepared admission boundary overflows"))?,
                PreparedAdmissionFactsFault::ExactBoundary => PROTECTED_FREE_SPACE_FLOOR
                    .checked_add(facts.allocation_unit)
                    .ok_or_else(|| String::from("prepared admission boundary overflows"))?,
                PreparedAdmissionFactsFault::Unsupported
                | PreparedAdmissionFactsFault::Replaced => {
                    unreachable!()
                }
            };
            Ok(facts)
        }
    }
}

struct OwnerRuntime {
    recording_producer: Option<recording_binding::RetainedRecordingProducer>,
    recording_restart_audit: Option<recording_audit::Pending>,
    publication_fences: publication_fence::Registry,
    legacy_adoption: Option<legacy_adoption_custody::RetainedLegacyAdoption>,
    native_readers: BTreeMap<VolumeIdentity, std::sync::Weak<NativeCapsuleData>>,
    native_reader_registry_input: Option<InputPermit>,
    resource_admission: Arc<ResourceAdmission>,
    resource_input: Option<InputPermit>,
    wal_preparation: Option<super::wal_admission::Collector>,
    wal_preparation_unconfirmed: bool,
    retirement_admission: Option<retirement_admission::Pending>,
    store: Option<OperationJournalStore>,
    capacity_provider: Option<CapacityProvider>,
    capacity_rebuild_readiness: CapacityRebuildReadiness,
    capacity_readiness_credit: Option<StartupCredit>,
    startup_error_credit: Option<StartupCredit>,
    reserve: Option<OwnedReserveAllocation>,
    volume_owner: Option<VolumeOwnershipCapability>,
    final_profile_guard: Option<WritableProfileGuard>,
    source_registry_credits: Vec<current_source_metadata::CurrentInput>,
    source_volumes: BTreeMap<VolumeIdentity, SourceVolumeBinding>,
    source_registrations:
        HashMap<crate::sample_sources::db::SourceRegistrationId, SourceAuthorityRecord>,
    retired_source_registrations:
        HashMap<crate::sample_sources::db::SourceRegistrationId, RetiredSourceAuthorityRecord>,
    issued_chunk_tickets: HashMap<Uuid, IssuedChunkTicket>,
    pending_chunk_releases: HashMap<Uuid, PendingChunkRelease>,
    #[cfg(test)]
    drop_observer: Option<OwnerRuntimeDropObserver>,
    #[cfg(test)]
    drop_probe: Option<OwnerRuntimeDropProbe>,
    #[cfg(test)]
    prepared_admission_id_allocations: Arc<AtomicUsize>,
    #[cfg(test)]
    synthetic_chunk_write_invocations: Arc<AtomicUsize>,
    #[cfg(test)]
    next_chunk_callback_witness_error: Option<String>,
    // Last: original ticket buckets are destroyed before their accumulated debit.
    issued_chunk_ticket_backing: Option<InputPermit>,
}

impl OwnerRuntime {
    fn new(store: OperationJournalStore) -> Result<Self, String> {
        let final_profile_guard = store
            .try_clone_profile_guard_for_owner()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            recording_producer: None,
            recording_restart_audit: None,
            publication_fences: publication_fence::Registry::new(),
            legacy_adoption: None,
            native_readers: BTreeMap::new(),
            native_reader_registry_input: None,
            resource_admission: ResourceAdmission::new(),
            resource_input: None,
            wal_preparation: None,
            wal_preparation_unconfirmed: false,
            retirement_admission: None,
            store: Some(store),
            capacity_provider: None,
            capacity_rebuild_readiness: CapacityRebuildReadiness::reconciliation_required(vec![
                String::from("capacity rebuild has not run"),
            ]),
            capacity_readiness_credit: None,
            startup_error_credit: None,
            reserve: None,
            volume_owner: None,
            final_profile_guard: Some(final_profile_guard),
            source_registry_credits: Vec::new(),
            source_volumes: BTreeMap::new(),
            source_registrations: HashMap::new(),
            retired_source_registrations: HashMap::new(),
            issued_chunk_tickets: HashMap::new(),
            issued_chunk_ticket_backing: None,
            pending_chunk_releases: HashMap::new(),
            #[cfg(test)]
            drop_observer: None,
            #[cfg(test)]
            drop_probe: None,
            #[cfg(test)]
            prepared_admission_id_allocations: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            synthetic_chunk_write_invocations: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            next_chunk_callback_witness_error: None,
        })
    }

    #[cfg(test)]
    fn new_with_id_counter(
        store: OperationJournalStore,
        prepared_admission_id_allocations: Arc<AtomicUsize>,
        synthetic_chunk_write_invocations: Arc<AtomicUsize>,
    ) -> Result<Self, String> {
        let mut runtime = Self::new(store)?;
        runtime.prepared_admission_id_allocations = prepared_admission_id_allocations;
        runtime.synthetic_chunk_write_invocations = synthetic_chunk_write_invocations;
        Ok(runtime)
    }

    #[cfg(all(test, target_os = "macos"))]
    fn fail_next_chunk_callback_witness_for_test(&mut self, reason: impl Into<String>) {
        self.next_chunk_callback_witness_error = Some(reason.into());
    }

    fn store(&self) -> &OperationJournalStore {
        self.store
            .as_ref()
            .expect("owner runtime store must remain installed")
    }

    fn volume_owner(&self) -> &VolumeOwnershipCapability {
        self.volume_owner
            .as_ref()
            .expect("owner runtime volume owner must be installed")
    }

    fn install_volume_owner(&mut self, volume_owner: VolumeOwnershipCapability) {
        debug_assert!(self.volume_owner.is_none());
        self.volume_owner = Some(volume_owner);
    }

    fn install_reserve(&mut self, reserve: OwnedReserveAllocation) {
        debug_assert!(self.reserve.is_none());
        self.reserve = Some(reserve);
    }

    fn begin_wal_preparation(
        &mut self,
        input: WalPreparationInput,
        debit: super::wal_admission::JournalInput,
    ) -> Result<WalPreparationHandle, OperationJournalOwnerError> {
        if self.wal_preparation.is_some() || self.wal_preparation_unconfirmed {
            return Err(WalPreparationError::Busy.into());
        }
        if std::time::Instant::now() >= input.deadline() {
            return Err(WalPreparationError::Deadline.into());
        }
        self.store().validate_for_admission()?;
        let acquisition = uuid::Uuid::parse_str(
            self.final_profile_guard
                .as_ref()
                .expect("original journal profile guard")
                .owner_identity(),
        )
        .map_err(|_| WalPreparationError::InvalidInput)?;
        let (collector, handle) = super::wal_admission::Collector::new(input, debit, acquisition)?;
        self.wal_preparation = Some(collector);
        self.poll_wal_preparation();
        Ok(handle)
    }
    fn poll_original_work(&mut self) {
        self.poll_recording_custody();
        self.poll_recording_restart_audit();
        self.poll_publication_completions();
        self.poll_wal_preparation();
    }
    fn poll_publication_completions(&self) {
        self.publication_fences.poll_completions();
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let Some(header) = self.publication_fences.original_input() else {
            return;
        };
        let shared = &header.shared;
        self.publication_fences.settle_no_effects(|completion| {
            let input = match shared.startup_input_with_publication(true) {
                Ok(input) => input,
                Err(_) => {
                    shared.mark_unavailable(startup_credit::REFUSAL);
                    return false;
                }
            };
            match store.settle_original_publication_no_effect(completion, &input) {
                Ok(_) => true,
                Err(error) => {
                    shared.mark_startup_unavailable(&error, &input);
                    false
                }
            }
        });
    }
    fn poll_wal_preparation(&mut self) {
        self.poll_retirement_admission();
        let done = self
            .wal_preparation
            .as_mut()
            .is_some_and(|collector| collector.poll());
        if done {
            let collector = self.wal_preparation.take().expect("original collector");
            self.wal_preparation_unconfirmed |= collector.is_unavailable();
            drop(collector);
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    fn admit_prepared(
        &mut self,
        prepared: PreparedAdmissionInput,
    ) -> Result<OperationId, OperationJournalOwnerError> {
        self.admit_prepared_with_funding(prepared, None)
    }

    fn admit_prepared_with_funding(
        &mut self,
        prepared: PreparedAdmissionInput,
        input: Option<&InputPermit>,
    ) -> Result<OperationId, OperationJournalOwnerError> {
        if matches!(
            prepared.args,
            UniversalCommandArgs::SourceRegistryRetirement(_)
        ) {
            return Err(PreparedAdmissionError::Unsupported {
                reason: "retirement must use the original acknowledgement consumer and qualified physical-family admission".into(),
            }.into());
        }
        prepared
            .validate_shape()
            .map_err(OperationJournalOwnerError::from)?;
        self.store()
            .validate_namespace_with_input(input, false)
            .map_err(OperationJournalOwnerError::from)?;
        if let UniversalCommandArgs::Recording(proposal) = &prepared.args {
            self.store()
                .validate_recording_capacity_binding_with_input(proposal, &prepared.plan, input)?;
        }

        // Re-read the durable ordinary-claim image for every request. Startup readiness is only
        // a bootstrap observation and cannot authorize a later request after new recovery rows
        // or settlement evidence arrive.
        if let Some(input) = input {
            #[cfg(test)]
            input.historical_consumer_test_gate();
            input.fund_historical_consumer().map_err(|error| {
                PreparedAdmissionError::RecoveryRequired {
                    reason: error.to_string(),
                }
            })?;
        }
        let snapshot = match input {
            Some(input) => self.store().rebuild_admission_snapshot_for_input(input),
            None => self
                .store()
                .rebuild_admission_snapshot()
                .map(super::store::FundedSnapshot::unfunded),
        }
        .map_err(|error| PreparedAdmissionError::RecoveryRequired {
            reason: error.to_string(),
        })?;
        match snapshot.readiness() {
            CapacityRebuildReadiness::ReconciliationRequired { diagnostics } => {
                return Err(PreparedAdmissionError::RecoveryRequired {
                    reason: bounded_diagnostics(diagnostics),
                }
                .into());
            }
            CapacityRebuildReadiness::RecoveryOnly { diagnostics }
                if !snapshot.proves_durable_zero() =>
            {
                return Err(PreparedAdmissionError::RecoveryRequired {
                    reason: bounded_diagnostics(diagnostics),
                }
                .into());
            }
            CapacityRebuildReadiness::Ready | CapacityRebuildReadiness::RecoveryOnly { .. } => {}
        }
        for claim in prepared.claims.claims() {
            if snapshot
                .claims()
                .iter()
                .any(|retained| retained.claim_id() == claim.claim_id())
            {
                return Err(PreparedAdmissionError::DuplicateClaim {
                    claim_id: claim.claim_id(),
                }
                .into());
            }
        }

        #[cfg(test)]
        if input.is_some_and(|input| {
            input
                .shared
                .provider_reconciliation_component
                .load(Ordering::Acquire)
        }) {
            // Original queued provider component only. No descriptor/witness bypass
            // can admit work: this hook always terminates Unsupported before IDs/SQL.
            let provider = CapacityProvider::unavailable();
            snapshot
                .install_provider_component(&provider)
                .map_err(|error| PreparedAdmissionError::RecoveryRequired {
                    reason: error.to_string(),
                })?;
            snapshot
                .validate_provider(&provider, input)
                .map_err(|error| PreparedAdmissionError::RecoveryRequired {
                    reason: error.to_string(),
                })?;
            return Err(PreparedAdmissionError::Unsupported {
                reason: "funded provider component verified; physical admission unsupported".into(),
            }
            .into());
        }

        #[cfg(all(test, target_os = "macos"))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission(prepared.facts_fault)
            .map_err(|reason| PreparedAdmissionError::DescriptorRefreshRejected { reason })?;
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission()
            .map_err(|reason| PreparedAdmissionError::DescriptorRefreshRejected { reason })?;
        let witness = self
            .live_admission_witness(observation)
            .map_err(|reason| PreparedAdmissionError::DescriptorRefreshRejected { reason })?;
        let provider =
            self.capacity_provider
                .as_ref()
                .ok_or_else(|| PreparedAdmissionError::Failed {
                    reason: String::from("capacity provider is not installed"),
                })?;
        // Reconcile the fresh durable image against the existing owner-thread ledger before
        // arithmetic. An exact image is a no-op; any changed/nonempty conflict remains held and
        // blocks admission without replacing live accounting.
        snapshot
            .validate_provider(provider, input)
            .map_err(|error| PreparedAdmissionError::RecoveryRequired {
                reason: format!("current durable/live capacity image differs: {error}"),
            })?;
        provider
            .validate_recovery_state()
            .map_err(PreparedAdmissionError::from)?;
        provider
            .validate_claim_identities_available(&prepared.claims)
            .map_err(PreparedAdmissionError::from)?;
        let construction_credit = input
            .map(|input| {
                OperationJournalStore::prepay_initial_record_construction(
                    &(
                        &prepared.intent,
                        &prepared.universal_intent,
                        &prepared.args,
                        &prepared.plan,
                        &prepared.claims,
                    ),
                    input,
                )
            })
            .transpose()?;
        let lease = match input {
            Some(input) => provider.admit_with_live_witness_for_input(
                &prepared.plan,
                &prepared.claims,
                witness,
                input,
            ),
            None => provider.admit_with_live_witness(&prepared.plan, &prepared.claims, witness),
        }
        .map_err(PreparedAdmissionError::from)?;

        let result = (|| {
            // Ordinary commands allocate their identity here; recording retains its frozen
            // identity. Everything above validates or creates one provisional owner lease.
            #[cfg(test)]
            self.prepared_admission_id_allocations
                .fetch_add(1, Ordering::Relaxed);
            let operation_id = if let UniversalCommandArgs::Recording(proposal) = &prepared.args {
                proposal.operation_id()
            } else {
                OperationId::new()
            };
            let record = prepared_record(operation_id, &prepared)?;
            let settlement = self
                .store()
                .settle_initial_claims_with_lease_for_input(
                    &record,
                    &prepared.claims,
                    provider,
                    lease,
                    input,
                )
                .map_err(|error| map_initial_settlement_error(error, &prepared.claims))?;
            let (outcome, retained_lease) = settlement.into_parts();
            match outcome {
                JournalInsertOutcome::Committed(_) => Ok(operation_id),
                JournalInsertOutcome::Failed(failure) => {
                    let claim_ids = prepared
                        .claims
                        .claims()
                        .iter()
                        .map(|claim| claim.claim_id())
                        .collect::<Vec<_>>();
                    if let Some(lease) = retained_lease {
                        drop(lease);
                        Err(PreparedAdmissionRetainedError::from_failure(failure, claim_ids).into())
                    } else {
                        Err(PreparedAdmissionRolledBackError {
                            operation_id,
                            reason: failure.error().to_string(),
                        }
                        .into())
                    }
                }
            }
        })();
        if let Some(credit) = construction_credit {
            if result.is_err() {
                input
                    .expect("funded construction")
                    .retain_historical_error(credit, None, None, None);
            }
        }
        result
    }

    fn chunk_rebuild_snapshot(
        &self,
    ) -> Result<super::store::OrdinaryClaimRebuildSnapshot, OperationJournalOwnerError> {
        let snapshot = self.store().rebuild_admission_snapshot().map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!("current durable chunk/claim image is not admissible: {error}"),
            }
        })?;
        match snapshot.readiness() {
            CapacityRebuildReadiness::ReconciliationRequired { diagnostics } => {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: format!(
                        "durable chunk/claim image requires reconciliation: {}",
                        bounded_diagnostics(diagnostics)
                    ),
                });
            }
            CapacityRebuildReadiness::Ready | CapacityRebuildReadiness::RecoveryOnly { .. } => {}
        }
        Ok(snapshot)
    }

    fn validate_chunk_operation_state(
        &self,
        operation_id: OperationId,
        action: &str,
    ) -> Result<(), OperationJournalOwnerError> {
        let operation = self
            .store()
            .get(operation_id)
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: format!("operation {operation_id} was not found for chunk {action}"),
            })?;
        if operation.phase != super::model::OperationPhase::IntentDurable
            || !matches!(
                operation.disposition,
                super::model::OperationDisposition::None
                    | super::model::OperationDisposition::RetryPending
            )
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: format!("operation {operation_id} is not eligible for chunk {action}"),
            });
        }
        if matches!(action, "claim" | "issuance") {
            if let Some(UniversalCommandArgs::Recording(proposal)) =
                operation.universal_command_args()
            {
                self.store().require_recording_capturing(proposal)?;
            }
            if self.recording_producer.as_ref().is_some_and(|retained| {
                retained.operation_id_for_claim_fence() == operation_id
                    && retained.closed_for_generic_claims()
            }) {
                return Err(OperationJournalOwnerError::RecordingProducerCustody(
                    RecordingProducerCustodyError::Changed,
                ));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn claim_chunk(
        &mut self,
        request: ChunkClaimRequest,
    ) -> Result<FundedChunkClaimRecord, OperationJournalOwnerError> {
        self.claim_chunk_with_funding(request, None)
    }

    fn claim_chunk_with_funding(
        &mut self,
        request: ChunkClaimRequest,
        input: Option<&InputPermit>,
    ) -> Result<FundedChunkClaimRecord, OperationJournalOwnerError> {
        request
            .validate()
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            })?;
        let existing = match input {
            Some(input) => self
                .store()
                .read_chunk_claim_for_input(request.chunk_id(), input),
            None => self
                .store()
                .read_chunk_claim(request.chunk_id())
                .map(|row| row.map(FundedChunkClaimRecord::unfunded)),
        }
        .map_err(OperationJournalOwnerError::from)?;
        if let Some(existing) = existing {
            if existing.matches_request(&request) {
                return Ok(existing);
            }
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk identity is already bound to different metadata"),
            });
        }
        if self.recording_producer.as_ref().is_some_and(|retained| {
            retained.operation_id_for_claim_fence() == request.operation_id()
                && retained.closed_for_generic_claims()
        }) {
            return Err(OperationJournalOwnerError::RecordingProducerCustody(
                RecordingProducerCustodyError::Changed,
            ));
        }
        self.publication_fences
            .close_for_mutation(request.operation_id())?;
        match input {
            Some(input) => self
                .store()
                .validate_chunk_operation_for_input(request.operation_id(), "claim", input)
                .map_err(|error| match error {
                    super::store::OperationJournalStoreError::ChunkClaim { reason } => {
                        OperationJournalOwnerError::ChunkClaim { reason }
                    }
                    error => error.into(),
                })?,
            None => self.validate_chunk_operation_state(request.operation_id(), "claim")?,
        }
        let mut write_scope = match input {
            Some(input) => Some(self.store().prepay_chunk_write(&request, input)?),
            None => {
                self.store().validate_for_admission()?;
                None
            }
        };
        let snapshot = match input {
            Some(input) => self.store().rebuild_admission_snapshot_for_input(input)?,
            None => super::store::FundedSnapshot::unfunded(self.chunk_rebuild_snapshot()?),
        };
        if let CapacityRebuildReadiness::ReconciliationRequired { diagnostics } =
            snapshot.readiness()
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "durable chunk/claim image requires reconciliation: {}",
                    bounded_diagnostics(diagnostics)
                ),
            });
        }
        let claims =
            request
                .inventory()
                .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                    reason: error.to_string(),
                })?;
        let plan = request
            .plan()
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            })?;
        if let (Some(input), Some(scope)) = (input, write_scope.as_mut()) {
            scope.retain_observation_credit(self.admission_observation_credit(input)?);
        }
        #[cfg(all(test, target_os = "macos"))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission(None)
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission()
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let witness = self
            .live_admission_witness(observation)
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("capacity provider is not installed"),
            }
        })?;
        snapshot
            .validate_provider(provider, input)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: format!("current durable/live capacity image differs: {error}"),
            })?;
        provider.validate_recovery_state().map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            }
        })?;
        provider
            .validate_claim_identities_available(&claims)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            })?;
        let lease = match input {
            Some(input) => {
                provider.admit_with_live_witness_for_input(&plan, &claims, witness, input)
            }
            None => provider.admit_with_live_witness(&plan, &claims, witness),
        }
        .map_err(|error| OperationJournalOwnerError::ChunkClaim {
            reason: error.to_string(),
        })?;
        match write_scope {
            Some(scope) => self
                .store()
                .persist_chunk_claim_with_lease_for_input(&request, provider, lease, scope),
            None => self
                .store()
                .persist_chunk_claim_with_lease(&request, provider, lease)
                .map(FundedChunkClaimRecord::unfunded),
        }
        .map_err(OperationJournalOwnerError::from)
    }

    fn issue_chunk(
        &mut self,
        chunk_id: ChunkId,
        operation_id: OperationId,
    ) -> Result<ChunkWritePermit, OperationJournalOwnerError> {
        self.issue_chunk_with_funding(chunk_id, operation_id, None)
    }

    fn issue_chunk_with_funding(
        &mut self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        input: Option<&InputPermit>,
    ) -> Result<ChunkWritePermit, OperationJournalOwnerError> {
        let record = match input {
            Some(input) => self.store().read_chunk_claim_for_input(chunk_id, input),
            None => self
                .store()
                .read_chunk_claim(chunk_id)
                .map(|r| r.map(FundedChunkClaimRecord::unfunded)),
        }
        .map_err(OperationJournalOwnerError::from)?
        .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
            reason: String::from("chunk claim was not found"),
        })?;
        if record.operation_id() != operation_id {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk claim belongs to a different operation"),
            });
        }
        match input {
            Some(input) => self
                .store()
                .validate_chunk_operation_for_input(operation_id, "issuance", input)
                .map_err(|error| match error {
                    super::store::OperationJournalStoreError::ChunkClaim { reason } => {
                        OperationJournalOwnerError::ChunkClaim { reason }
                    }
                    error => error.into(),
                })?,
            None => self.validate_chunk_operation_state(operation_id, "issuance")?,
        }
        match record.state() {
            ChunkClaimState::Issued => {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: String::from(
                        "chunk claim is already Issued; a dropped permit cannot be reissued",
                    ),
                });
            }
            ChunkClaimState::Released => {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: String::from("released chunk claim cannot be issued"),
                });
            }
            ChunkClaimState::Claimed => {}
        }
        if self.recording_producer.as_ref().is_some_and(|retained| {
            retained.operation_id_for_claim_fence() == operation_id
                && retained.closed_for_generic_claims()
        }) {
            return Err(OperationJournalOwnerError::RecordingProducerCustody(
                RecordingProducerCustodyError::Changed,
            ));
        }
        self.publication_fences.close_for_mutation(operation_id)?;
        let mut write_scope = match input {
            Some(input) => Some(self.store().prepay_chunk_issuance(
                operation_id,
                chunk_id,
                input,
            )?),
            None => {
                self.store().validate_for_admission()?;
                None
            }
        };
        let snapshot = match input {
            Some(input) => self.store().rebuild_admission_snapshot_for_input(input)?,
            None => super::store::FundedSnapshot::unfunded(self.chunk_rebuild_snapshot()?),
        };
        // Both original owned identities and the actual retained hash buckets are
        // paid before native observation, UUID generation or the durable CAS.
        let ticket_credit = input
            .map(|input| {
                input.reserve_historical_scratch(1024 + std::mem::size_of::<IssuedChunkTicket>())
            })
            .transpose()?;
        let permit_credit = input
            .map(|input| {
                input.reserve_historical_scratch(
                    1024 + std::mem::size_of::<ChunkWritePermit>()
                        + std::mem::size_of::<ChunkWriteEntry>(),
                )
            })
            .transpose()?;
        self.prepay_chunk_ticket_growth(input)?;
        if let (Some(input), Some(scope)) = (input, write_scope.as_mut()) {
            scope.retain_observation_credit(self.admission_observation_credit(input)?);
        }
        #[cfg(all(test, target_os = "macos"))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission(None)
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission()
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let witness = self
            .live_admission_witness(observation)
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("capacity provider is not installed"),
            }
        })?;
        snapshot
            .validate_provider(provider, input)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: format!("current durable/live capacity image differs: {error}"),
            })?;
        provider.validate_recovery_state().map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            }
        })?;
        record
            .validate_issuance(provider, &witness)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            })?;
        let issue_id = Uuid::new_v4();
        let receipt = match write_scope {
            Some(scope) => self.store().issue_chunk_claim_funded(
                chunk_id,
                operation_id,
                issue_id,
                scope,
                &record,
            ),
            None => self
                .store()
                .issue_chunk_claim(chunk_id, operation_id, issue_id),
        }
        .map_err(OperationJournalOwnerError::from)?;
        let permit = ChunkWritePermit::from_issue_receipt_funded(
            receipt,
            permit_credit.map(InputPermit::into_startup_credit),
        );
        self.issued_chunk_tickets.insert(
            permit.issue_id(),
            IssuedChunkTicket {
                unused_release: None,
                chunk_id: permit.chunk_id(),
                operation_id: permit.operation_id(),
                identity: permit.identity().clone(),
                live_output: None,
                live_target: None,
                live_source_fences: Vec::new(),
                _credit: ticket_credit.map(InputPermit::into_startup_credit),
            },
        );
        Ok(permit)
    }

    fn prepay_chunk_ticket_growth(
        &mut self,
        input: Option<&InputPermit>,
    ) -> Result<(), OperationJournalOwnerError> {
        let Some(input) = input else {
            return Ok(());
        };
        if self.issued_chunk_tickets.len() < self.issued_chunk_tickets.capacity() {
            return Ok(());
        }
        // Repository-pinned Rust 1.93/hashbrown: power-of-two buckets, <=7/8
        // load, one control byte per bucket and a <=16-byte control group.
        // Capture actual old capacity. Prepay the complete next table before
        // try_reserve, including alignment/header slack, even with old unfunded
        // compatibility entries. Retain all growth debits through table teardown.
        let bytes = self
            .issued_chunk_tickets
            .capacity()
            .checked_add(1)
            .and_then(usize::checked_next_power_of_two)
            .and_then(|n| n.checked_mul(2))
            .map(|n| n.max(4))
            .and_then(|n| n.checked_mul(std::mem::size_of::<(Uuid, IssuedChunkTicket)>() + 1))
            .and_then(|n| n.checked_add(64))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let credit = input.reserve_historical_scratch(bytes)?;
        self.issued_chunk_tickets
            .try_reserve(1)
            .map_err(|_| OperationJournalStoreError::HistoricalClaimResourceBound)?;
        match self.issued_chunk_ticket_backing.as_mut() {
            Some(backing) => backing.absorb_historical_scratch(credit),
            None => self.issued_chunk_ticket_backing = Some(credit),
        }
        Ok(())
    }

    fn retain_recording_chunk(
        &self,
        chunk: &RecordingCapturedChunk,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
    ) -> Result<RecordingChunkReceipt, OperationJournalOwnerError> {
        let source = self
            .source_registrations
            .get(&registration.registration_id)
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: "recording source is not registered".into(),
            })?;
        if source.receipt != registration
            || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: "recording source registration is stale/draining".into(),
            });
        }
        let profile = self.final_profile_guard.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: "recording profile owner is unavailable".into(),
            }
        })?;
        source_pin.validate_for_issuance(&source.request, profile)?;
        let _source_effect = source_pin.begin_effect()?;
        let operation = self.store().get(chunk.operation_id())?.ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: "recording operation absent".into(),
            }
        })?;
        let Some(UniversalCommandArgs::Recording(proposal)) = operation.universal_command_args()
        else {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: "operation is not recording".into(),
            });
        };
        chunk
            .validate_against(proposal)
            .map_err(|e| OperationJournalOwnerError::ChunkClaim {
                reason: e.to_string(),
            })?;
        let epoch = u64::try_from(registration.lease_identity.epoch()).map_err(|_| {
            OperationJournalOwnerError::ChunkClaim {
                reason: "recording lease epoch invalid".into(),
            }
        })?;
        let current =
            recording_binding::matches_original_destination(proposal, &source.request, epoch);
        if !current {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: "recording source differs from frozen participant".into(),
            });
        }
        let claim = self
            .store()
            .read_chunk_claim(ChunkId::from_uuid(chunk.chunk_id()).map_err(|e| {
                OperationJournalOwnerError::ChunkClaim {
                    reason: e.to_string(),
                }
            })?)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: "recording chunk has no admitted durable claim".into(),
            })?;
        if !matches!(
            claim.state(),
            ChunkClaimState::Claimed | ChunkClaimState::Issued
        ) || claim.operation_id() != chunk.operation_id()
            || claim.participant_id() != chunk.work_key().participant_id()
            || claim.logical_offset() != chunk.offset()
            || claim.logical_length() != chunk.bytes().len() as u64
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: "recording bytes differ from admitted chunk range".into(),
            });
        }
        self.validate_recording_custody_append(chunk)?;
        self.validate_chunk_operation_state(chunk.operation_id(), "recording retention")?;
        let snapshot = self.chunk_rebuild_snapshot()?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: "recording capacity owner unavailable".into(),
            }
        })?;
        provider.validate_rebuilt_claims(&snapshot).map_err(|e| {
            OperationJournalOwnerError::ChunkClaim {
                reason: e.to_string(),
            }
        })?;
        provider
            .validate_recovery_state()
            .map_err(|e| OperationJournalOwnerError::ChunkClaim {
                reason: e.to_string(),
            })?;
        self.publication_fences
            .close_for_mutation(chunk.operation_id())?;
        let receipt = self.store().retain_recording_chunk(chunk)?;
        source_pin.validate_for_issuance(&source.request, profile)?;
        Ok(receipt)
    }

    fn issue_live_chunk_effect(
        &mut self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        spec: LiveChunkEffectSpec,
    ) -> Result<LiveChunkEffectGrant, OperationJournalOwnerError> {
        let source = self
            .source_registrations
            .get(&registration.registration_id)
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk source registration is not retained"),
            })?;
        if source.receipt != registration
            || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk source registration is not current"),
            });
        }
        let profile_guard = self.final_profile_guard.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk profile owner is unavailable"),
            }
        })?;
        source_pin.validate_for_issuance(&source.request, profile_guard)?;
        let lease_epoch = u64::try_from(registration.lease_identity.epoch()).map_err(|_| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk source lease epoch is invalid"),
            }
        })?;
        if !spec.source_fences.iter().any(|fence| {
            fence.root_identity() == source.request.media_root().identity()
                && fence.database_identity() == source.request.database_file_identity()
                && fence.lease_epoch() == lease_epoch
        }) {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk source fences omit its registered lease"),
            });
        }
        let record = self
            .store()
            .read_chunk_claim(chunk_id)
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk claim was not found"),
            })?;
        if record.operation_id() != operation_id
            || record.logical_offset() != spec.offset
            || spec.logical_bytes > record.logical_length()
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk effect does not match the claimed range"),
            });
        }
        let operation = self
            .store()
            .get(operation_id)
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk operation was not found"),
            })?;
        let participants = operation
            .universal_intent
            .as_ref()
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("live chunk operation has no participant set"),
            })?
            .participants();
        let fence = &spec.source_fences[0];
        let participant_is_retained = participants
            .descriptors()
            .iter()
            .any(|descriptor| descriptor.id() == record.participant_id());
        let source_is_retained = participants.descriptors().iter().any(|descriptor| {
            descriptor.source_fence().is_some_and(|stored| {
                stored.source_id() == fence.source_id()
                    && stored.root_identity() == fence.root_identity()
                    && stored.database_identity() == fence.database_identity()
                    && stored.lifecycle_generation() == fence.lifecycle_generation()
                    && fence.source_revision() == Some(stored.source_revision())
                    && stored.lease_epoch() == fence.lease_epoch()
            })
        });
        if !participant_is_retained || !source_is_retained {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from(
                    "live chunk source or participant is absent from the operation",
                ),
            });
        }
        if let Some(UniversalCommandArgs::Recording(proposal)) = operation.universal_command_args()
        {
            let captured = self
                .store()
                .get_recording_chunk(operation_id, chunk_id.as_uuid())?
                .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                    reason: "recording bytes were not retained before dispatch".into(),
                })?;
            if !recording_effect_matches(proposal, &captured, &spec)
                || captured.work_key().participant_id() != record.participant_id()
            {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: "recording effect differs from exact durable bytes/root/output".into(),
                });
            }
        }
        let output_key = record.output_key().to_owned();
        let permit = self.issue_chunk(chunk_id, operation_id)?;
        let ticket = self
            .issued_chunk_tickets
            .get_mut(&permit.issue_id())
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("issued chunk ticket disappeared before live binding"),
            })?;
        ticket.live_output = Some(spec.output);
        ticket.live_target = Some(spec.target);
        ticket.live_source_fences = spec.source_fences.clone();
        Ok(LiveChunkEffectGrant {
            permit,
            source_pin,
            spec,
            output_key,
        })
    }

    fn checkpoint_and_grant_staging(
        &mut self,
        target: PreparedPublicationTarget,
        charge_selector: publication_charge::Selector,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        input: InputPermit,
    ) -> Result<StagingPublicationGrant, OperationJournalOwnerError> {
        validate_publication_target(&target)?;
        let validation = target.validation();
        self.resource_admission.validate_open().map_err(|error| {
            OperationJournalOwnerError::StagingGrant {
                reason: format!("original publication admission is closed: {error:?}"),
            }
        })?;
        let native = self
            .capture_native_read_capsule(target.volume())
            .map_err(|error| OperationJournalOwnerError::StagingGrant {
                reason: format!("original publication native reader unavailable: {error:?}"),
            })?;
        let source = self
            .source_registrations
            .get(&registration.registration_id)
            .ok_or_else(|| OperationJournalOwnerError::StagingGrant {
                reason: String::from("source registration is not retained"),
            })?;
        if source.receipt != registration
            || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
        {
            return Err(OperationJournalOwnerError::StagingGrant {
                reason: String::from("source registration is not current"),
            });
        }
        let profile_guard = self.final_profile_guard.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::StagingGrant {
                reason: String::from("profile ownership is unavailable"),
            }
        })?;
        source_pin.validate_for_issuance(&source.request, profile_guard)?;
        let lease_epoch = u64::try_from(registration.lease_identity.epoch()).map_err(|_| {
            OperationJournalOwnerError::StagingGrant {
                reason: String::from("source lease epoch is invalid"),
            }
        })?;
        let [fence] = validation.source_fences() else {
            return Err(OperationJournalOwnerError::StagingGrant {
                reason: String::from("staging validation must bind one live source fence"),
            });
        };
        if fence.root_identity() != source.request.media_root().identity()
            || fence.database_identity() != source.request.database_file_identity()
            || fence.lease_epoch() != lease_epoch
        {
            return Err(OperationJournalOwnerError::StagingGrant {
                reason: String::from("staging source fence differs from live registration"),
            });
        }
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::StagingGrant {
                reason: "original publication capacity provider is unavailable".into(),
            }
        })?;
        let bytes = match validation.outcome() {
            crate::file_operation_owner::EffectOutcome::Applied { evidence } => {
                evidence.validated_staging().map(|proof| proof.bytes())
            }
            _ => None,
        }
        .ok_or_else(|| OperationJournalOwnerError::StagingGrant {
            reason: "original publication target lacks validated staging bytes".into(),
        })?;
        let fence = self.publication_fences.issue_for_source(
            validation.operation_id(),
            registration.registration_id,
            &input.shared,
        )?;
        let charge = publication_charge::bind(
            self.store(),
            provider,
            charge_selector,
            validation.operation_id(),
            target.volume(),
            bytes,
        )?;
        let issuer = self
            .publication_fences
            .issuance_observation(&fence)
            .ok_or_else(|| OperationJournalOwnerError::StagingGrant {
                reason: "original publication issuer observation is unavailable".into(),
            })?;
        let completion_command =
            target
                .publication_command()
                .map_err(|_| OperationJournalOwnerError::StagingGrant {
                    reason: "original completion command is invalid".into(),
                })?;
        let completion_bytes = completion_command.canonical_bytes().map_err(|_| {
            OperationJournalOwnerError::StagingGrant {
                reason: "original completion command is invalid".into(),
            }
        })?;
        let completion_fingerprint = *blake3::hash(&completion_bytes).as_bytes();
        let chunk = match charge_selector {
            publication_charge::Selector::Ordinary(_) => None,
            publication_charge::Selector::Chunk(chunk) => Some(chunk),
        };
        let binding =
            self.store()
                .capture_publication_binding(super::store::PublicationBindingCapture {
                    canonical_command: &completion_bytes,
                    validation,
                    charge: &charge,
                    chunk,
                    issuer,
                })?;
        drop(completion_bytes);
        drop(completion_command);
        let checkpoint_result = self
            .store()
            .checkpoint_staging_result_funded(validation, &input);
        let checkpoint: StagingCheckpointAck = self
            .publication_write_result(checkpoint_result)?
            .ok_or_else(|| OperationJournalOwnerError::StagingGrant {
                reason: String::from("replayed staging checkpoint cannot issue a grant"),
            })?;
        source_pin.validate_for_issuance(&source.request, profile_guard)?;
        validate_publication_target(&target)?;
        let grant_result = self
            .store()
            .reserve_staging_grant(checkpoint, validation, binding, &input);
        self.publication_write_result(grant_result)?;
        let output = match validation.outcome() {
            crate::file_operation_owner::EffectOutcome::Applied { evidence } => evidence
                .validated_staging()
                .expect("validated original staging")
                .output()
                .as_uuid(),
            _ => unreachable!("validated original staging"),
        };
        fence.bind_reserved_output(output)?;
        validate_publication_target(&target)?;
        provider
            .validate_publication_charge(&charge)
            .map_err(|error| OperationJournalOwnerError::StagingGrant {
                reason: format!("original publication charge changed before reply: {error}"),
            })?;
        Ok(StagingPublicationGrant {
            target,
            completion_fingerprint,
            fence,
            charge,
            native,
            input,
            source_pin,
            resource_admission: Arc::clone(&self.resource_admission),
        })
    }

    fn request_cancel_before_publish(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<JournalCancelRequestReceipt, OperationJournalOwnerError> {
        self.publication_fences
            .close_for_cancellation(operation_id)?;
        self.store()
            .request_cancel_before_publish(operation_id, expected_sequence, observation)
            .map_err(Into::into)
    }

    fn publication_write_result<T>(
        &self,
        result: Result<T, OperationJournalStoreError>,
    ) -> Result<T, OperationJournalOwnerError> {
        if matches!(
            &result,
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. }
                | OperationJournalStoreError::StagingNamespaceOutcomeUncertain)
        ) {
            self.resource_admission.invalidate();
        }
        result.map_err(Into::into)
    }

    fn checkpoint_live_chunk_staging(
        &mut self,
        chunk_id: ChunkId,
        issue_id: Uuid,
        validation: FileOperationResult,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
    ) -> Result<ChunkStagingReceipt, OperationJournalOwnerError> {
        let ticket = self.issued_chunk_tickets.get(&issue_id).ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk checkpoint has no live issued ticket"),
            }
        })?;
        let crate::file_operation_owner::EffectOutcome::Applied { evidence } = validation.outcome()
        else {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk staging validation did not succeed"),
            });
        };
        let proof =
            evidence
                .validated_staging()
                .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                    reason: String::from("chunk validation has no physical staging proof"),
                })?;
        if ticket.unused_release.is_some()
            || ticket.chunk_id != chunk_id
            || ticket.operation_id != validation.operation_id()
            || ticket.identity.participant_id() != validation.participant_id()
            || ticket.live_output != Some(proof.output())
            || ticket.live_target != Some(validation.target_registration())
            || ticket.live_source_fences != validation.source_fences()
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk staging proof differs from its live issued ticket"),
            });
        }
        let source = self
            .source_registrations
            .get(&registration.registration_id)
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk source registration is not retained"),
            })?;
        if source.receipt != registration
            || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk source registration is not current"),
            });
        }
        let profile_guard = self.final_profile_guard.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk profile ownership is unavailable"),
            }
        })?;
        source_pin.validate_for_issuance(&source.request, profile_guard)?;
        let lease_epoch = u64::try_from(registration.lease_identity.epoch()).map_err(|_| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk source lease epoch is invalid"),
            }
        })?;
        let [fence] = validation.source_fences() else {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk validation requires one source fence"),
            });
        };
        if fence.root_identity() != source.request.media_root().identity()
            || fence.database_identity() != source.request.database_file_identity()
            || fence.lease_epoch() != lease_epoch
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk source fence differs from live registration"),
            });
        }
        self.publication_fences
            .close_for_mutation(validation.operation_id())?;
        let receipt = self
            .store()
            .checkpoint_chunk_staging(chunk_id, issue_id, &validation)?;
        self.issued_chunk_tickets.remove(&issue_id);
        Ok(receipt)
    }

    fn begin_chunk_write(
        &mut self,
        permit: ChunkWritePermit,
        identity: ChunkWriteIdentity,
        logical_bytes: u64,
    ) -> Result<(), OperationJournalOwnerError> {
        let issue_id = permit.issue_id();
        let ticket = self.issued_chunk_tickets.get(&issue_id).ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk write ticket is not held by this owner"),
            }
        })?;
        if ticket.unused_release.is_some()
            || ticket.chunk_id != permit.chunk_id()
            || ticket.operation_id != permit.operation_id()
            || ticket.identity != *permit.identity()
            || ticket.identity != identity
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk write ticket identity conflicts with owner state"),
            });
        }
        if logical_bytes == 0 || logical_bytes > permit.logical_length() {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk write budget {logical_bytes} exceeds ticket allowance {}",
                    permit.logical_length()
                ),
            });
        }
        self.publication_fences
            .close_for_mutation(permit.operation_id())?;
        let record = self
            .store()
            .read_chunk_claim(permit.chunk_id())
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk claim was not found for write ticket"),
            })?;
        if record.operation_id() != permit.operation_id()
            || record.state() != ChunkClaimState::Issued
            || record.issue_id() != Some(issue_id)
            || record.write_identity() != *permit.identity()
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk write ticket no longer matches durable Issued row"),
            });
        }
        self.validate_chunk_operation_state(permit.operation_id(), "entry")?;
        self.store().validate_for_admission()?;
        let snapshot = self.chunk_rebuild_snapshot()?;
        #[cfg(all(test, target_os = "macos"))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission(None)
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self
            .refresh_live_descriptor_capacity_for_admission()
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let witness = self
            .live_admission_witness(observation.clone())
            .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: String::from("capacity provider is not installed"),
            }
        })?;
        provider
            .validate_rebuilt_claims(&snapshot)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: format!("current durable/live capacity image differs: {error}"),
            })?;
        provider.validate_recovery_state().map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            }
        })?;
        provider
            .validate_durable_chunk_issuance(&record, &witness)
            .map_err(|error| OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            })?;
        drop(witness);

        // The registry removal is the owner-thread single-use transition.  The entry is kept
        // local to this call and the synthetic callback below receives no filesystem authority.
        self.issued_chunk_tickets.remove(&issue_id);
        let entry = permit.consume(&identity, logical_bytes).map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: error.to_string(),
            }
        })?;
        // The registry transition consumes the ticket before the callback boundary.  Source
        // lease liveness and filesystem identities can change on another owner between these
        // checks, so the second witness revalidation may fail conservatively after consumption.
        #[cfg(test)]
        let callback_witness = if let Some(reason) = self.next_chunk_callback_witness_error.take() {
            Err(reason)
        } else {
            self.live_admission_witness(observation)
        };
        #[cfg(not(test))]
        let callback_witness = self.live_admission_witness(observation);
        let callback_witness =
            callback_witness.map_err(|reason| OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "live chunk entry authority became stale before callback: {reason}"
                ),
            })?;
        run_synthetic_chunk_write(
            &entry,
            &callback_witness,
            #[cfg(test)]
            &self.synthetic_chunk_write_invocations,
        )
        .map_err(|reason| OperationJournalOwnerError::ChunkClaim { reason })?;
        drop(entry);
        Ok(())
    }

    fn release_chunk(
        &mut self,
        permit: ChunkWritePermit,
        release_id: Uuid,
    ) -> Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError> {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release identity is nil"),
            });
        }
        let chunk_id = permit.chunk_id();
        let operation_id = permit.operation_id();
        let issue_id = permit.issue_id();
        let Some(ticket) = self.issued_chunk_tickets.get_mut(&issue_id) else {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release ticket is not held by this owner"),
            });
        };
        if ticket.chunk_id != chunk_id
            || ticket.operation_id != operation_id
            || ticket.identity != *permit.identity()
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release ticket identity conflicts with owner state"),
            });
        }
        if self.pending_chunk_releases.contains_key(&release_id) {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release identity is already pending"),
            });
        }
        if ticket
            .unused_release
            .is_some_and(|retained| retained != release_id)
        {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("original unused release identity conflicts"),
            });
        }
        // The moved original permit, never an ID-only request, proves this issuance unused.
        // Retain its exact proof inline even if publication custody refuses charge release.
        ticket.unused_release = Some(release_id);
        self.publication_fences
            .close_for_charge_release(operation_id)?;
        self.issued_chunk_tickets.remove(&issue_id);
        self.pending_chunk_releases.insert(
            release_id,
            PendingChunkRelease {
                chunk_id,
                operation_id,
                issue_id,
                release_id,
            },
        );
        let authorization =
            ChunkUnusedReleaseAuthorization::new(chunk_id, operation_id, issue_id, release_id);
        let receipt = self
            .store()
            .release_chunk_claim(chunk_id, operation_id, Some(authorization), release_id)
            .map_err(OperationJournalOwnerError::from)?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk release {release_id} committed but capacity provider is not installed"
                ),
            }
        })?;
        provider.release_claim(&receipt).map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk release {release_id} committed but provider accounting remains retained: {error}"
                ),
            }
        })?;
        self.pending_chunk_releases.remove(&release_id);
        Ok(receipt)
    }

    fn retry_chunk_release(
        &mut self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
    ) -> Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError> {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release identity is nil"),
            });
        }
        self.publication_fences
            .close_for_charge_release(operation_id)?;
        let pending_issue_id = match self.pending_chunk_releases.get(&release_id) {
            Some(pending)
                if pending.chunk_id == chunk_id
                    && pending.operation_id == operation_id
                    && pending.release_id == release_id =>
            {
                Some(pending.issue_id)
            }
            Some(_) => {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: String::from(
                        "chunk release retry identity conflicts with pending proof",
                    ),
                });
            }
            None => None,
        };
        let current = self
            .store()
            .read_chunk_claim(chunk_id)
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk claim was not found for release retry"),
            })?;
        if current.operation_id() != operation_id {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk claim belongs to a different operation"),
            });
        }
        let authorization = match current.state() {
            ChunkClaimState::Released => {
                if current.release_id() != Some(release_id) {
                    return Err(OperationJournalOwnerError::ChunkClaim {
                        reason: String::from("durable chunk release identity conflicts"),
                    });
                }
                None
            }
            ChunkClaimState::Issued => {
                let issue_id =
                    current
                        .issue_id()
                        .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                            reason: String::from("Issued chunk has no issue identity"),
                        })?;
                let original_unused =
                    self.issued_chunk_tickets
                        .get(&issue_id)
                        .is_some_and(|ticket| {
                            ticket.chunk_id == chunk_id
                                && ticket.operation_id == operation_id
                                && ticket.identity == current.write_identity()
                                && ticket.unused_release == Some(release_id)
                        });
                if pending_issue_id.is_some_and(|pending| pending != issue_id)
                    || (pending_issue_id.is_none() && !original_unused)
                {
                    return Err(OperationJournalOwnerError::ChunkClaim {
                        reason: String::from(
                            "Issued chunk has no retained unused release authority",
                        ),
                    });
                }
                if original_unused {
                    self.issued_chunk_tickets.remove(&issue_id);
                    self.pending_chunk_releases.insert(
                        release_id,
                        PendingChunkRelease {
                            chunk_id,
                            operation_id,
                            issue_id,
                            release_id,
                        },
                    );
                }
                Some(ChunkUnusedReleaseAuthorization::new(
                    chunk_id,
                    operation_id,
                    issue_id,
                    release_id,
                ))
            }
            ChunkClaimState::Claimed => {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: String::from(
                        "Claimed chunk requires cancel_chunk before release retry",
                    ),
                });
            }
        };
        let receipt = self
            .store()
            .release_chunk_claim(chunk_id, operation_id, authorization, release_id)
            .map_err(OperationJournalOwnerError::from)?;
        let provider = self.capacity_provider.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk release {release_id} is durable but capacity provider is not installed"
                ),
            }
        })?;
        provider.release_claim(&receipt).map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!("chunk release {release_id} remains retained for retry: {error}"),
            }
        })?;
        self.pending_chunk_releases.remove(&release_id);
        Ok(receipt)
    }

    fn cancel_chunk(
        &mut self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
    ) -> Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError> {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk release identity is nil"),
            });
        }
        self.publication_fences
            .close_for_charge_release(operation_id)?;
        let current = self
            .store()
            .read_chunk_claim(chunk_id)
            .map_err(OperationJournalOwnerError::from)?
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: String::from("chunk claim was not found for cancellation"),
            })?;
        if current.operation_id() != operation_id || current.state() != ChunkClaimState::Claimed {
            return Err(OperationJournalOwnerError::ChunkClaim {
                reason: String::from("only a Claimed chunk can be cancelled by id"),
            });
        }
        let receipt = self
            .store()
            .release_chunk_claim(chunk_id, operation_id, None, release_id)
            .map_err(OperationJournalOwnerError::from)?;
        let provider = self
            .capacity_provider
            .as_ref()
            .ok_or_else(|| OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk cancellation {release_id} committed but capacity provider is not installed"
                ),
            })?;
        provider.release_claim(&receipt).map_err(|error| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!(
                    "chunk cancellation {release_id} committed but provider accounting remains retained: {error}"
                ),
            }
        })?;
        Ok(receipt)
    }

    fn live_admission_witness(
        &self,
        observation: LiveDescriptorCapacityObservation,
    ) -> Result<LiveAdmissionWitness<'_>, String> {
        let profile_guard = self
            .final_profile_guard
            .as_ref()
            .ok_or_else(|| String::from("owner profile guard is not installed"))?;
        self.validate_source_authorities_for_admission(profile_guard)?;
        self.validate_legacy_volume_bindings()
            .map_err(|e| e.to_string())?;
        let profile_owner = self
            .volume_owner
            .as_ref()
            .ok_or_else(|| String::from("profile volume ownership is not installed"))?;
        let profile_reserve = self
            .reserve
            .as_ref()
            .ok_or_else(|| String::from("profile recovery reserve is not installed"))?;
        let profile_volume = profile_owner.volume().clone();
        let mut volume_owners = vec![profile_owner];
        let mut reserves = vec![profile_reserve];
        for (volume, binding) in &self.source_volumes {
            if *volume == profile_volume {
                continue;
            }
            if binding.readiness != SourceVolumeBindingReadiness::Complete {
                return Err(String::from(
                    "a retained source volume binding is incomplete",
                ));
            }
            volume_owners.push(binding.owner.as_ref().ok_or_else(|| {
                String::from("a complete source binding has no retained volume owner")
            })?);
            reserves.push(binding.reserve.as_ref().ok_or_else(|| {
                String::from("a complete source binding has no retained reserve")
            })?);
        }
        if let Some(volumes) = self
            .legacy_adoption
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
        {
            for entry in &volumes.entries {
                if let legacy_adoption_volumes::LegacyVolumeEntry::Owned {
                    owner, reserve, ..
                } = entry
                {
                    volume_owners.push(owner.as_ref().ok_or("legacy volume owner is incomplete")?);
                    reserves.push(reserve.as_ref().ok_or("legacy reserve is incomplete")?);
                }
            }
        }
        Ok(LiveAdmissionWitness {
            observation,
            _profile_guard: profile_guard,
            _volume_owners: volume_owners,
            _reserves: reserves,
        })
    }

    fn validate_source_authorities_for_admission(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), String> {
        for (registration_id, record) in &self.source_registrations {
            if !matches!(record.readiness, SourceRegistrationReadiness::Registered) {
                return Err(format!(
                    "source authority {registration_id:?} is not active for capacity admission"
                ));
            }
            record
                .request
                .validate_registered_boundary(profile_guard)
                .map_err(|error| {
                    format!(
                        "source authority {registration_id:?} is no longer valid for capacity admission: {error}"
                    )
                })?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn register_source_authority(
        &mut self,
        input: SourceAuthorityRegistrationRequest,
    ) -> Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError> {
        self.register_source_authority_funded(None, input)
    }

    fn validate_current_registry_credit(
        &self,
        credit: Option<&current_source_metadata::CurrentInput>,
    ) -> Result<(), OperationJournalOwnerError> {
        let Some(credit) = credit else {
            return Ok(());
        };
        if credit.class != InputClass::Ordinary
            || credit.publication
            || credit.bytes < current_source_metadata::MIN_RETAINED_CURRENT_BYTES
            || self.source_registry_credits.len() >= current_source_metadata::MAX_CURRENT_CREDITS
        {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: "original Current registry metadata credit is unavailable".into(),
            });
        }
        Ok(())
    }

    fn register_source_authority_funded(
        &mut self,
        // Parameters drop in reverse order: the original request/confirmation must drop first.
        current_input: Option<current_source_metadata::CurrentInput>,
        input: SourceAuthorityRegistrationRequest,
    ) -> Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError> {
        let request = input.request;
        let registration_id = request.registration_id();
        if let Some(existing) = self.source_registrations.get(&registration_id) {
            if existing.request.matches_registration(&request) {
                return match &existing.readiness {
                    SourceRegistrationReadiness::Registered => Ok(existing.receipt.clone()),
                    SourceRegistrationReadiness::Draining => Ok(existing.receipt.clone()),
                    SourceRegistrationReadiness::Registering => Err(
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "registration {registration_id:?} is still being installed"
                            ),
                            receipt: None,
                        },
                    ),
                    SourceRegistrationReadiness::RecoveryRequired { stage, reason } => Err(
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "registration {registration_id:?} remains in {stage}: {reason}"
                            ),
                            receipt: None,
                        },
                    ),
                };
            }
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from(
                    "registration identity was reused with different source evidence",
                ),
            });
        }
        if let Some(retired) = self.retired_source_registrations.get(&registration_id) {
            if retired.matches_request(&request) {
                return Ok(retired.receipt.clone());
            }
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from(
                    "retired registration identity conflicts with retained evidence",
                ),
            });
        }

        // Retired and partial records retain custody and count toward the same bound.
        // Exact duplicate readback above remains possible at full retention.
        if self.source_registrations.len().checked_add(self.retired_source_registrations.len())
            .is_none_or(|n| n >= crate::sample_sources::db::source_registry_handoff::MAX_SOURCE_REGISTRY_CARRIERS)
        {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("source authority carrier retention limit reached"),
            });
        }

        self.validate_current_registry_credit(current_input.as_ref())?;

        let profile_guard = self
            .final_profile_guard
            .as_ref()
            .ok_or_else(|| OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("owner profile capability is unavailable"),
            })
            .and_then(|guard| {
                guard.try_clone().map_err(|error| {
                    OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                        reason: format!("owner profile capability could not be cloned: {error}"),
                        receipt: None,
                    }
                })
            })?;
        request
            .validate_consuming_boundary(&profile_guard)
            .map_err(OperationJournalOwnerError::from)?;
        let profile_volume_id = self
            .volume_owner
            .as_ref()
            .ok_or_else(|| OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("profile volume ownership is unavailable"),
            })?
            .volume()
            .clone();
        if self.reserve.is_none() {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("profile recovery reserve is unavailable"),
            });
        }
        let roots = [request.media_root(), request.database_root()];
        let mut volume_ids = BTreeSet::new();
        let mut allocation_units = BTreeMap::new();
        let mut work = Vec::new();
        for root in roots {
            let descriptor = root.try_clone_descriptor().map_err(|error| {
                OperationJournalOwnerError::SourceAuthorityConflict {
                    reason: format!("source volume descriptor could not be duplicated: {error}"),
                }
            })?;
            let facts = descriptor_capacity_facts(&descriptor).map_err(|error| {
                OperationJournalOwnerError::SourceAuthorityConflict {
                    reason: format!("source volume facts are unavailable: {error}"),
                }
            })?;
            if let Some(previous) =
                allocation_units.insert(facts.identity.clone(), facts.allocation_unit)
                && previous != facts.allocation_unit
            {
                return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                    reason: String::from(
                        "same source volume reported conflicting allocation units",
                    ),
                });
            }
            if !volume_ids.insert(facts.identity.clone()) {
                continue;
            }
            if let Some(binding) = self.source_volumes.get_mut(&facts.identity) {
                if binding.allocation_unit != Some(facts.allocation_unit) {
                    return Err(
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "source volume {identity:?} allocation unit changed",
                                identity = facts.identity
                            ),
                            receipt: None,
                        },
                    );
                }
                if binding.readiness != SourceVolumeBindingReadiness::Complete {
                    return Err(
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "source volume {identity:?} has incomplete retained binding",
                                identity = facts.identity
                            ),
                            receipt: None,
                        },
                    );
                }
                if facts.identity != profile_volume_id {
                    let Some(owner) = binding.owner.as_ref() else {
                        return Err(
                            OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                                reason: format!(
                                    "source volume {identity:?} has no complete ownership binding",
                                    identity = facts.identity
                                ),
                                receipt: None,
                            },
                        );
                    };
                    let Some(reserve) = binding.reserve.as_mut() else {
                        return Err(
                            OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                                reason: format!(
                                    "source volume {identity:?} has no complete reserve binding",
                                    identity = facts.identity
                                ),
                                receipt: None,
                            },
                        );
                    };
                    owner.revalidate().map_err(|error| {
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "source volume {identity:?} ownership is no longer current: {error}",
                                identity = facts.identity
                            ),
                            receipt: None,
                        }
                    })?;
                    reserve.view(&profile_guard).revalidate().map_err(|error| {
                        OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                            reason: format!(
                                "source volume {identity:?} reserve is no longer current: {error}",
                                identity = facts.identity
                            ),
                            receipt: None,
                        }
                    })?;
                }
            }
            work.push((facts.identity, root));
        }

        // Consume the source-owner proof before acquiring a lock, creating a reserve, or
        // persisting a binding. Every later physical effect revalidates this same deadline-bound
        // proof and converts it to a drain marker on partial failure.
        let mut proof = Some(request.accept_confirmation(input.confirmation)?);
        if let Some(credit) = current_input.as_ref() {
            self.source_registry_credits.push(credit.clone());
        }
        let volume_identities = volume_ids.iter().cloned().collect::<Vec<_>>();
        let receipt = SourceAuthorityRegistrationReceipt::new(
            registration_id,
            request.lease_identity().clone(),
            volume_identities,
        );
        for volume in &volume_ids {
            let binding = self
                .source_volumes
                .entry(volume.clone())
                .or_insert_with(|| SourceVolumeBinding {
                    reserve: None,
                    owner: None,
                    #[cfg(all(test, unix))]
                    _current_before_root_drop: None,
                    _current_root: None,
                    #[cfg(all(test, unix))]
                    _current_after_root_drop: None,
                    _current_volume_failure: Default::default(),
                    _current_reserve_failure: Default::default(),
                    allocation_unit: None,
                    readiness: SourceVolumeBindingReadiness::Incomplete,
                    registrations: BTreeSet::new(),
                    _current_input: current_input.clone(),
                });
            binding.allocation_unit = allocation_units.get(volume).copied();
            if *volume == profile_volume_id {
                binding.readiness = SourceVolumeBindingReadiness::Complete;
            }
            binding.registrations.insert(registration_id);
        }
        self.source_registrations.insert(
            registration_id,
            SourceAuthorityRecord {
                metadata_registration: crate::sample_sources::db::source_registry_handoff::MetadataRegistrationCustody::new(),
                metadata_writer: None,
                request: Arc::clone(&request),
                receipt: receipt.clone(),
                drain_marker: None,
                readiness: SourceRegistrationReadiness::Registering,
                _current_input: current_input.clone(),
            },
        );

        for (volume, root) in work {
            let scope = match request.reserve_use(
                root,
                proof
                    .as_ref()
                    .expect("registration proof remains until installation completes"),
                &profile_guard,
            ) {
                Ok(scope) => scope,
                Err(error) => {
                    return self.retain_source_recovery(
                        registration_id,
                        "volume-ownership-scope",
                        error.to_string(),
                        proof.take(),
                    );
                }
            };
            if let Err(error) = super::capacity_volume_owner::validate_current_source_scope(&scope)
            {
                return self.retain_source_recovery(
                    registration_id,
                    "volume-ownership-native-scope",
                    error.to_string(),
                    proof.take(),
                );
            }
            if volume == profile_volume_id
                || self.source_volumes.get(&volume).is_some_and(|binding| {
                    binding.readiness == SourceVolumeBindingReadiness::Complete
                        && binding.owner.is_some()
                        && binding.reserve.is_some()
                })
            {
                continue;
            }
            if let Err(error) = proof
                .as_ref()
                .expect("registration proof remains until installation completes")
                .validate_for_physical_use()
            {
                return self.retain_source_recovery(
                    registration_id,
                    "before-volume-ownership",
                    error.to_string(),
                    proof.take(),
                );
            }
            let original_root = match root.try_clone_for_owned_capability() {
                Ok(root) => root,
                Err(error) => {
                    return self.retain_source_recovery(
                        registration_id,
                        "volume-ancestor-retention",
                        error.to_string(),
                        proof.take(),
                    );
                }
            };
            self.source_volumes
                .get_mut(&volume)
                .expect("source volume placeholder was inserted")
                ._current_root = Some(original_root);
            // The readonly clone must not extend the original confirmation deadline.
            if let Err(error) = scope.validate_for_bootstrap() {
                return self.retain_source_recovery(
                    registration_id,
                    "after-volume-ancestor-retention",
                    error.to_string(),
                    proof.take(),
                );
            }
            let owner = match super::capacity_volume_owner::acquire_current_for_scope(
                &scope,
                &mut self
                    .source_volumes
                    .get_mut(&volume)
                    .expect("source volume placeholder was inserted")
                    ._current_volume_failure,
            ) {
                Ok(owner) => owner,
                Err(()) => {
                    return self.retain_source_recovery(
                        registration_id,
                        "volume-ownership",
                        String::from(
                            "original source volume acquisition retained affected native backing",
                        ),
                        proof.take(),
                    );
                }
            };
            self.source_volumes
                .get_mut(&volume)
                .expect("source volume placeholder was inserted")
                .owner = Some(owner);
            if let Err(error) = proof
                .as_ref()
                .expect("registration proof remains until installation completes")
                .validate_for_physical_use()
            {
                return self.retain_source_recovery(
                    registration_id,
                    "before-reserve-binding",
                    error.to_string(),
                    proof.take(),
                );
            }
            let owner = match self
                .source_volumes
                .get(&volume)
                .and_then(|binding| binding.owner.as_ref())
                .expect("source volume owner was installed")
                .try_clone_for_context()
            {
                Ok(owner) => owner,
                Err(error) => {
                    return self.retain_source_recovery(
                        registration_id,
                        "owner-context",
                        format!("source volume owner could not be retained: {error}"),
                        proof.take(),
                    );
                }
            };
            let reserve_failure = &self
                .source_volumes
                .get(&volume)
                .expect("source volume placeholder was inserted")
                ._current_reserve_failure;
            match self
                .store()
                .acquire_owned_current_reserve(owner, &scope, reserve_failure)
            {
                Ok(reserve) => {
                    let binding = self
                        .source_volumes
                        .get_mut(&volume)
                        .expect("source volume placeholder was inserted");
                    binding.reserve = Some(reserve);
                    binding.readiness = SourceVolumeBindingReadiness::Complete;
                    #[cfg(all(test, unix))]
                    if self.store().current_reopen_fixture() {
                        if let Err(reason) =
                            self.current_same_generation_reopen_for_test(&volume, &scope)
                        {
                            return self.retain_source_recovery(
                                registration_id,
                                "test-only-existing-current-reopen",
                                reason,
                                proof.take(),
                            );
                        }
                    }
                }
                Err(SourceReserveBindingFailure {
                    error,
                    volume_owner,
                    reserve,
                }) => {
                    let binding = self
                        .source_volumes
                        .get_mut(&volume)
                        .expect("source volume placeholder was inserted");
                    binding.owner = Some(volume_owner);
                    binding.reserve = reserve;
                    return self.retain_source_recovery(
                        registration_id,
                        "reserve-binding",
                        error.to_string(),
                        proof.take(),
                    );
                }
            }
        }
        let drain_marker = proof
            .take()
            .expect("registration proof remains until installation completes")
            .into_drain_marker();
        if let Some(record) = self.source_registrations.get_mut(&registration_id) {
            record.drain_marker = Some(drain_marker);
            record.metadata_registration.activate()?;
            record.readiness = SourceRegistrationReadiness::Registered;
        }
        Ok(receipt)
    }

    fn retain_source_recovery(
        &mut self,
        registration_id: crate::sample_sources::db::SourceRegistrationId,
        stage: &'static str,
        mut reason: String,
        proof: Option<crate::sample_sources::db::SourceAuthorityUseProof>,
    ) -> Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError> {
        let record = self
            .source_registrations
            .get_mut(&registration_id)
            .expect("source registration placeholder was inserted");
        // Persist at most 512 UTF-8 bytes; native diagnostics are not a retained input channel.
        if reason.len() > 512 {
            let mut end = 512;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        let reason = reason.as_str().to_owned();
        record.drain_marker = proof.map(|proof| proof.into_drain_marker());
        record.metadata_registration.revoke();
        record.readiness = SourceRegistrationReadiness::RecoveryRequired {
            stage,
            reason: reason.clone(),
        };
        Err(
            OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                reason: format!("registration {registration_id:?} at {stage}: {reason}"),
                receipt: Some(record.receipt.clone()),
            },
        )
    }

    fn authenticate_source_registry_metadata(
        &mut self,
        candidate: crate::sample_sources::db::SourceRegistryMetadataCandidate,
    ) -> Result<crate::sample_sources::db::SourceRegistryMetadataPermit, OperationJournalOwnerError>
    {
        let record = self
            .source_registrations
            .get_mut(&candidate.receipt.registration_id)
            .ok_or_else(|| OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("metadata handoff has no current registered source"),
            })?;
        if !matches!(record.readiness, SourceRegistrationReadiness::Registered)
            || !Arc::ptr_eq(&record.request, &candidate.request)
            || record.receipt.registration_id != candidate.receipt.registration_id
            || record.receipt.lease_identity != candidate.receipt.lease_identity
            || record.receipt.volume_identities != candidate.receipt.volume_identities
        {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from(
                    "metadata handoff differs from completed original registration",
                ),
            });
        }
        let subject = candidate.observation().subject();
        let source_id = subject.source_id().to_owned();
        let lifecycle = subject.lifecycle_generation();
        let writer = candidate.writer.clone();
        if record.metadata_writer.as_ref().is_some_and(|bound| {
            bound.source_id != source_id
                || bound.lifecycle != lifecycle
                || !bound.writer.ptr_eq(&writer)
        }) {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("metadata handoff replaced the original registered writer"),
            });
        }
        let profile = self.final_profile_guard.as_ref().ok_or_else(|| {
            OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("metadata consumer profile ownership unavailable"),
            }
        })?;
        let permit = candidate.into_permit(profile, record.metadata_registration.token())?;
        if record.metadata_writer.is_none() {
            record.metadata_writer = Some(SourceRegistryWriterBinding {
                source_id,
                lifecycle,
                writer,
            });
        }
        Ok(permit)
    }

    fn begin_source_drain(
        &mut self,
        receipt: SourceAuthorityRegistrationReceipt,
    ) -> Result<(), OperationJournalOwnerError> {
        let Some(record) = self.source_registrations.get_mut(&receipt.registration_id) else {
            if self
                .retired_source_registrations
                .get(&receipt.registration_id)
                .is_some_and(|retired| retired.receipt == receipt)
            {
                return Ok(());
            }
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("source registration is not retained by the journal owner"),
            });
        };
        if record.receipt != receipt {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("source drain receipt does not match retained registration"),
            });
        }
        if matches!(&record.readiness, SourceRegistrationReadiness::Registering) {
            return Err(
                OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                    reason: String::from("source registration is still being installed"),
                    receipt: None,
                },
            );
        }
        if matches!(&record.readiness, SourceRegistrationReadiness::Draining) {
            return Ok(());
        }
        let Some(marker) = record.drain_marker.as_ref() else {
            return Err(
                OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                    reason: String::from("source registration has no retained drain marker"),
                    receipt: None,
                },
            );
        };
        self.publication_fences
            .close_for_source(receipt.registration_id)?;
        if let Err(error) = record.request.begin_drain(marker.clone()) {
            return Err(OperationJournalOwnerError::from(error));
        }
        record.drain_marker.take();
        record.metadata_registration.revoke();
        record.readiness = SourceRegistrationReadiness::Draining;
        Ok(())
    }

    fn finalize_source_retirement(
        &mut self,
        receipt: SourceAuthorityRegistrationReceipt,
        acknowledgement: SourceAuthorityRetirementAck,
    ) -> Result<(), OperationJournalOwnerError> {
        if let Some(retired) = self
            .retired_source_registrations
            .get(&receipt.registration_id)
        {
            if retired.receipt == receipt {
                return Ok(());
            }
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("source retirement receipt conflicts with retained evidence"),
            });
        }
        let Some(record) = self.source_registrations.get(&receipt.registration_id) else {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from("source registration is not retained by the journal owner"),
            });
        };
        if record.receipt != receipt {
            return Err(OperationJournalOwnerError::SourceAuthorityConflict {
                reason: String::from(
                    "source retirement receipt does not match retained registration",
                ),
            });
        }
        self.publication_fences
            .close_for_source(receipt.registration_id)?;
        record
            .request
            .finalize_retirement(acknowledgement)
            .map_err(OperationJournalOwnerError::from)?;
        let record = self
            .source_registrations
            .remove(&receipt.registration_id)
            .expect("source registration was checked above");
        for volume in &record.receipt.volume_identities {
            let remove = if let Some(binding) = self.source_volumes.get_mut(volume) {
                binding.registrations.remove(&receipt.registration_id);
                // Keep complete external owner/reserve bindings idle after the last logical
                // source retires. Dropping them would leave a dirty marker and make a same-volume
                // source reopen impossible until the deferred clean-handoff contract exists.
                binding.registrations.is_empty()
                    && binding.owner.is_none()
                    && binding.reserve.is_none()
            } else {
                false
            };
            if remove {
                self.source_volumes.remove(volume);
            }
        }
        self.retired_source_registrations.insert(
            receipt.registration_id,
            RetiredSourceAuthorityRecord {
                media_root_identity: record.request.media_root().identity().to_owned(),
                database_root_identity: record.request.database_root().identity().to_owned(),
                database_file_identity: record.request.database_file_identity().to_owned(),
                lease_identity: record.request.lease_identity().clone(),
                receipt: receipt.clone(),
                _current_input: record._current_input.clone(),
            },
        );
        Ok(())
    }

    fn revalidate_capacity_boundary(&mut self) -> Result<PhysicalReserveProof, String> {
        let volume_owner = self
            .volume_owner
            .as_ref()
            .ok_or_else(|| String::from("volume ownership capability is not installed"))?;
        volume_owner
            .revalidate()
            .map_err(|error| error.to_string())?;
        let profile_guard = self
            .final_profile_guard
            .as_ref()
            .ok_or_else(|| String::from("owner profile guard is not installed"))?;
        let reserve = self
            .reserve
            .as_mut()
            .ok_or_else(|| String::from("owned reserve is not installed"))?;
        let proof = reserve
            .view(profile_guard)
            .revalidate_partial_and_prove()
            .map_err(|error| error.to_string())?;
        if proof.volume() != volume_owner.volume() {
            return Err(String::from(
                "physical reserve proof volume differs from the retained owner volume",
            ));
        }
        let minimum = self
            .store()
            .minimum_recovery_reserve_bytes(volume_owner.volume())?;
        if !proof.non_sparse() || proof.physical_bytes() < minimum {
            return Err(format!(
                "physical reserve proof does not meet the retained minimum: {} bytes, need at least {minimum}",
                proof.physical_bytes()
            ));
        }
        Ok(proof)
    }

    /// Complete the explicit orderly owner shutdown boundary.  The marker is attempted only
    /// after every retained source and durable ledger proof has passed; any marker uncertainty
    /// fences this runtime and returns no clean receipt.
    fn handoff_volume_to_clean_owner(&mut self) -> Result<(), String> {
        if self.publication_fences.unresolved() {
            self.resource_admission.invalidate();
        }
        if !self.resource_admission.close_and_wait() {
            return Err("publication resource use remains unknown or unsettled".into());
        }
        if self.wal_preparation.is_some() || self.wal_preparation_unconfirmed {
            return Err("original WAL preparation cleanup remains unconfirmed".into());
        }
        let volume_owner = self
            .volume_owner
            .as_ref()
            .ok_or_else(|| String::from("volume ownership capability is not installed"))?;
        volume_owner
            .revalidate()
            .map_err(|error| error.to_string())?;
        if !self.source_registrations.is_empty() {
            return Err(String::from(
                "source registrations remain active during clean volume handoff",
            ));
        }
        for (volume, binding) in &self.source_volumes {
            if !matches!(binding.readiness, SourceVolumeBindingReadiness::Complete)
                || !binding.registrations.is_empty()
            {
                return Err(format!(
                    "source volume {volume:?} retains incomplete or active registration state"
                ));
            }
        }
        self.capacity_provider
            .as_ref()
            .ok_or_else(|| String::from("capacity provider is not installed"))?
            .prove_no_live_charges()
            .map_err(|error| error.to_string())?;
        self.validate_legacy_volume_bindings()
            .map_err(|e| e.to_string())?;
        self.store()
            .prove_clean_handoff_durable()
            .map_err(|error| error.to_string())?;
        let profile_guard = self
            .final_profile_guard
            .as_ref()
            .ok_or_else(|| String::from("owner profile guard is not installed"))?;
        {
            let reserve = self
                .reserve
                .as_mut()
                .ok_or_else(|| String::from("owned reserve is not installed"))?;
            // `revalidate_and_prove` captures the retained descriptor's current identity and
            // allocation unit before returning the non-serializable full-floor proof.
            let proof = reserve
                .view(profile_guard)
                .revalidate_and_prove()
                .map_err(|error| error.to_string())?;
            if proof.volume() != volume_owner.volume()
                || !proof.non_sparse()
                || proof.physical_bytes() < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            {
                return Err(String::from(
                    "profile reserve does not meet the full protected physical floor",
                ));
            }
        }
        for (volume, binding) in &mut self.source_volumes {
            if *volume == *volume_owner.volume() {
                continue;
            }
            let owner = binding
                .owner
                .as_ref()
                .ok_or_else(|| format!("source volume {volume:?} has no retained owner"))?;
            owner.revalidate().map_err(|error| error.to_string())?;
            let reserve = binding
                .reserve
                .as_mut()
                .ok_or_else(|| format!("source volume {volume:?} has no retained reserve"))?;
            let proof = reserve
                .view(profile_guard)
                .revalidate_and_prove()
                .map_err(|error| error.to_string())?;
            if proof.volume() != volume
                || !proof.non_sparse()
                || proof.physical_bytes() < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            {
                return Err(format!(
                    "source volume {volume:?} does not meet the full protected physical floor"
                ));
            }
        }
        // Every preflighted retained volume crosses the same fenced marker boundary.  A failure
        // after an earlier volume was confirmed clean is intentionally not rolled back: those
        // owners remain retired and the remaining runtime stays conservative.
        let profile_volume = volume_owner.volume().clone();
        for (volume, binding) in &mut self.source_volumes {
            if *volume == profile_volume {
                continue;
            }
            let owner = binding
                .owner
                .as_mut()
                .ok_or_else(|| format!("source volume {volume:?} has no retained owner"))?;
            owner.mark_clean_after_handoff().map_err(|error| {
                format!("source volume {volume:?} clean handoff failed: {error}")
            })?;
        }
        if let Some(volumes) = self
            .legacy_adoption
            .as_mut()
            .and_then(|s| s.volumes.as_mut())
        {
            for entry in &mut volumes.entries {
                if let legacy_adoption_volumes::LegacyVolumeEntry::Owned {
                    owner,
                    reserve,
                    volume,
                    ..
                } = entry
                {
                    let reserve = reserve
                        .as_mut()
                        .ok_or("legacy reserve remains incomplete")?;
                    let proof = reserve
                        .view(profile_guard)
                        .revalidate_and_prove()
                        .map_err(|e| e.to_string())?;
                    if proof.volume() != volume
                        || !proof.non_sparse()
                        || proof.physical_bytes()
                            < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
                    {
                        return Err(
                            "legacy reserve cannot prove its complete protected floor".into()
                        );
                    }
                    owner
                        .as_mut()
                        .ok_or("legacy volume owner remains incomplete")?
                        .mark_clean_after_handoff()
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        let volume_owner = self
            .volume_owner
            .as_mut()
            .ok_or_else(|| String::from("volume ownership capability is not installed"))?;
        volume_owner
            .revalidate()
            .map_err(|error| error.to_string())?;
        volume_owner
            .mark_clean_after_handoff()
            .map_err(|error| error.to_string())?;
        // The marker is now the only durable handoff result. Fence all in-memory capacity state
        // before the normal drop order releases reserve and owner descriptors.
        self.capacity_provider.take();
        self.reserve.take();
        Ok(())
    }

    /// Observe every retained reserve descriptor while its owner and profile authorities remain
    /// borrowed on this worker thread. The maps are local until every volume succeeds, so a
    /// replacement, unsupported descriptor, or allocation-unit mismatch cannot publish a partial
    /// fact set or alter the provider ledger/readiness.
    fn refresh_live_descriptor_capacity(
        &mut self,
    ) -> Result<LiveDescriptorCapacityObservation, String> {
        #[cfg(test)]
        {
            self.refresh_live_descriptor_capacity_inner(None, false)
        }
        #[cfg(not(test))]
        {
            self.refresh_live_descriptor_capacity_inner(false)
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    fn refresh_live_descriptor_capacity_for_admission(
        &mut self,
        fault: Option<PreparedAdmissionFactsFault>,
    ) -> Result<LiveDescriptorCapacityObservation, String> {
        if let Some(fault) = fault {
            let reader = move |reserve: &mut OwnedReserveAllocation,
                               profile_guard: &WritableProfileGuard|
                  -> Result<VolumeFacts, String> {
                prepared_admission_fault_facts(fault, reserve, profile_guard)
            };
            self.refresh_live_descriptor_capacity_inner(Some(&reader), true)
        } else {
            self.refresh_live_descriptor_capacity_inner(None, true)
        }
    }

    #[cfg(not(all(test, target_os = "macos")))]
    fn refresh_live_descriptor_capacity_for_admission(
        &mut self,
    ) -> Result<LiveDescriptorCapacityObservation, String> {
        self.refresh_live_descriptor_capacity_inner(
            #[cfg(test)]
            None,
            true,
        )
    }

    fn refresh_live_descriptor_capacity_inner(
        &mut self,
        #[cfg(test)] facts_reader: Option<
            &dyn Fn(
                &mut OwnedReserveAllocation,
                &WritableProfileGuard,
            ) -> Result<VolumeFacts, String>,
        >,
        require_full_floor: bool,
    ) -> Result<LiveDescriptorCapacityObservation, String> {
        let profile_guard = self
            .final_profile_guard
            .as_ref()
            .ok_or_else(|| String::from("owner profile guard is not installed"))?;
        let profile_owner = self
            .volume_owner
            .as_ref()
            .ok_or_else(|| String::from("profile volume ownership is not installed"))?;
        let profile_reserve = self
            .reserve
            .as_mut()
            .ok_or_else(|| String::from("profile recovery reserve is not installed"))?;
        #[cfg(test)]
        let (profile_volume, profile_facts, profile_provenance) =
            observe_retained_descriptor_inner(
                profile_owner,
                profile_reserve,
                profile_guard,
                require_full_floor,
                facts_reader,
            )?;
        #[cfg(not(test))]
        let (profile_volume, profile_facts, profile_provenance) =
            observe_retained_descriptor_inner(
                profile_owner,
                profile_reserve,
                profile_guard,
                require_full_floor,
            )?;

        let mut facts = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        facts.insert(profile_volume.clone(), profile_facts.clone());
        provenance.insert(profile_volume.clone(), profile_provenance);

        if let Some(binding) = self.source_volumes.get(&profile_volume) {
            if binding.readiness != SourceVolumeBindingReadiness::Complete {
                return Err(format!(
                    "profile source volume {profile_volume:?} has an incomplete retained binding"
                ));
            }
            if binding.allocation_unit != Some(profile_facts.allocation_unit) {
                return Err(format!(
                    "profile source volume {profile_volume:?} allocation unit differs from the retained reserve"
                ));
            }
        }

        for (volume, binding) in &mut self.source_volumes {
            if *volume == profile_volume {
                continue;
            }
            if binding.readiness != SourceVolumeBindingReadiness::Complete {
                return Err(format!(
                    "source volume {volume:?} has an incomplete retained binding"
                ));
            }
            let owner = binding.owner.as_ref().ok_or_else(|| {
                format!("source volume {volume:?} has no retained ownership capability")
            })?;
            let reserve = binding.reserve.as_mut().ok_or_else(|| {
                format!("source volume {volume:?} has no retained reserve descriptor")
            })?;
            #[cfg(test)]
            let (observed_volume, observed_facts, observed_provenance) =
                observe_retained_descriptor_inner(
                    owner,
                    reserve,
                    profile_guard,
                    require_full_floor,
                    facts_reader,
                )?;
            #[cfg(not(test))]
            let (observed_volume, observed_facts, observed_provenance) =
                observe_retained_descriptor_inner(
                    owner,
                    reserve,
                    profile_guard,
                    require_full_floor,
                )?;
            if observed_volume != *volume {
                return Err(format!(
                    "source volume {volume:?} reserve descriptor resolved to {observed_volume:?}"
                ));
            }
            if binding.allocation_unit != Some(observed_facts.allocation_unit) {
                return Err(format!(
                    "source volume {volume:?} allocation unit differs from its retained binding"
                ));
            }
            facts.insert(observed_volume.clone(), observed_facts);
            provenance.insert(observed_volume, observed_provenance);
        }

        self.observe_legacy_reserve_facts(
            &mut facts,
            &mut provenance,
            require_full_floor,
            #[cfg(test)]
            facts_reader,
        )?;
        Ok(LiveDescriptorCapacityObservation { facts, provenance })
    }

    #[cfg(all(test, target_os = "macos"))]
    fn refresh_live_descriptor_capacity_with_reader(
        &mut self,
        facts_reader: &dyn Fn(
            &mut OwnedReserveAllocation,
            &WritableProfileGuard,
        ) -> Result<VolumeFacts, String>,
    ) -> Result<LiveDescriptorCapacityObservation, String> {
        self.refresh_live_descriptor_capacity_inner(Some(facts_reader), false)
    }

    #[cfg(all(test, target_os = "macos"))]
    fn rebuild_capacity(&mut self) -> Result<(), String> {
        self.rebuild_capacity_with_funding(None)
    }

    fn rebuild_capacity_with_funding(&mut self, input: Option<&InputPermit>) -> Result<(), String> {
        let _proof = self.revalidate_capacity_boundary()?;
        let _descriptor_observation = self.refresh_live_descriptor_capacity()?;
        self.rebuild_capacity_image_with_funding(input)
    }

    fn rebuild_capacity_image_with_funding(
        &mut self,
        input: Option<&InputPermit>,
    ) -> Result<(), String> {
        if self.capacity_provider.is_none() {
            self.capacity_provider = Some(
                match input {
                    Some(input) => CapacityProvider::unavailable_for_input(input),
                    None => Ok(CapacityProvider::unavailable()),
                }
                .map_err(|_| String::from(startup_credit::REFUSAL))?,
            );
        }
        // Keep the exact original provider installed through every new refusal path.
        let provider = self.capacity_provider.as_ref().unwrap();
        let snapshot = match match input {
            Some(input) => self
                .store()
                .rebuild_ordinary_startup_snapshot_for_input(input),
            None => self
                .store()
                .rebuild_ordinary_claim_snapshot()
                .map(super::store::FundedSnapshot::unfunded),
        } {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Some(input) = input {
                    let bytes = startup_credit::display_bytes(&error)
                        .ok_or_else(|| String::from(startup_credit::REFUSAL))?;
                    self.startup_error_credit = Some(
                        input
                            .reserve_historical_scratch(bytes)
                            .map_err(|_| String::from(startup_credit::REFUSAL))?
                            .into_startup_credit(),
                    );
                }
                return Err(error.to_string());
            }
        };
        let mut readiness_credit = match input {
            Some(input) => {
                let bytes = snapshot
                    .readiness()
                    .diagnostics()
                    .iter()
                    .try_fold(
                        std::mem::size_of::<CapacityRebuildReadiness>(),
                        |total, text| {
                            total
                                .checked_add(std::mem::size_of::<String>())?
                                .checked_add(text.len().checked_mul(2)?)
                        },
                    )
                    .ok_or_else(|| String::from(startup_credit::REFUSAL))?;
                Some(
                    input
                        .reserve_historical_scratch(bytes)
                        .map_err(|_| String::from(startup_credit::REFUSAL))?
                        .into_startup_credit(),
                )
            }
            None => None,
        };
        let mut readiness = snapshot.readiness().clone();
        if readiness.can_install() {
            if let Err(error) = match input {
                Some(input) => snapshot.install_provider_for_input(provider, input),
                None => snapshot.install_provider_component_compatibility(provider),
            } {
                let replacement_credit = if let Some(input) = input {
                    let message =
                        format_args!("ordinary-claim provider rebuild was rejected: {error}");
                    let bytes = startup_credit::display_bytes(&message)
                        .and_then(|n| {
                            n.checked_add(
                                std::mem::size_of::<Vec<String>>() + std::mem::size_of::<String>(),
                            )
                        })
                        .ok_or_else(|| String::from(startup_credit::REFUSAL))?;
                    Some(
                        input
                            .reserve_historical_scratch(bytes)
                            .map_err(|_| String::from(startup_credit::REFUSAL))?
                            .into_startup_credit(),
                    )
                } else {
                    None
                };
                readiness = CapacityRebuildReadiness::reconciliation_required(vec![format!(
                    "ordinary-claim provider rebuild was rejected: {error}"
                )]);
                readiness_credit = replacement_credit;
            }
        }
        if !readiness.diagnostics().is_empty() {
            tracing::warn!(
                diagnostics = ?readiness.diagnostics(),
                "ordinary claim capacity rebuild retained recovery-only evidence"
            );
        }
        self.capacity_rebuild_readiness = readiness;
        self.capacity_readiness_credit = readiness_credit;
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn capacity_rebuild_readiness(&self) -> &CapacityRebuildReadiness {
        &self.capacity_rebuild_readiness
    }

    #[cfg(test)]
    fn capacity_ledger_snapshot_for_test(&self) -> CapacityLedgerTestSnapshot {
        let provider = self
            .capacity_provider
            .as_ref()
            .expect("capacity provider must be installed for the diagnostic snapshot");
        CapacityLedgerTestSnapshot {
            claims: provider.ledger_claims(),
            dispositions: provider.ledger_dispositions(),
            next_token: provider.next_token(),
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    fn install_drop_observer(
        &mut self,
        observer: OwnerRuntimeDropObserver,
        probe: OwnerRuntimeDropProbe,
    ) {
        self.drop_observer = Some(observer);
        self.drop_probe = Some(probe);
    }
}

fn bounded_diagnostics(diagnostics: &[String]) -> String {
    let mut reason = diagnostics
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if reason.is_empty() {
        reason.push_str("journal recovery evidence is not ready");
    }
    reason.chars().take(512).collect()
}

fn run_synthetic_chunk_write(
    entry: &ChunkWriteEntry,
    witness: &LiveAdmissionWitness<'_>,
    #[cfg(test)] callback_invocations: &AtomicUsize,
) -> Result<(), String> {
    if entry.logical_length() == 0 || entry.allocated_bytes() == 0 {
        return Err(String::from("chunk write entry has no bounded allocation"));
    }
    let _ = (
        entry.chunk_id(),
        entry.operation_id(),
        entry.issue_id(),
        entry.identity(),
        witness.facts(),
    );
    #[cfg(test)]
    callback_invocations.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn prepared_record(
    operation_id: OperationId,
    prepared: &PreparedAdmissionInput,
) -> Result<StoredOperationRecord, OperationJournalOwnerError> {
    if matches!(prepared.args, UniversalCommandArgs::SourceLegacyAdoption(_)) {
        return Err(PreparedAdmissionError::Unsupported {
            reason:
                "legacy adoption requires the original-owner capacity and durable admission bridge"
                    .into(),
        }
        .into());
    }
    if matches!(
        prepared.args,
        UniversalCommandArgs::SourceDerivedLayout(_)
            | UniversalCommandArgs::DecodedEdit(_)
            | UniversalCommandArgs::DecodedExport(_)
            | UniversalCommandArgs::DecodedExportBatch(_)
    ) {
        return Err(PreparedAdmissionError::Unsupported {
            reason: String::from(
                "full-progress-only command arguments have no initial-intent codec",
            ),
        }
        .into());
    }
    let construct = if matches!(prepared.args, UniversalCommandArgs::Recording(_)) {
        StoredOperationRecord::new_v17_recording
    } else if matches!(
        prepared.args,
        UniversalCommandArgs::SourceRegistryRetirement(_)
    ) {
        StoredOperationRecord::new_v18_source_retirement
    } else {
        StoredOperationRecord::new_v16_command
    };
    Ok(construct(
        operation_id,
        prepared.intent.clone(),
        prepared.universal_intent.clone(),
        prepared.args.clone(),
        prepared.plan.clone(),
        prepared.timestamp_unix_ms,
    ))
}

fn map_initial_settlement_error(
    error: InitialClaimSettlementError,
    claims: &AdmissionClaimInventory,
) -> OperationJournalOwnerError {
    match error {
        InitialClaimSettlementError::Store(error) => error.into(),
        InitialClaimSettlementError::CapacityPlan(error) => PreparedAdmissionError::Unsupported {
            reason: error.to_string(),
        }
        .into(),
        InitialClaimSettlementError::Claims(error) => PreparedAdmissionError::Unsupported {
            reason: error.to_string(),
        }
        .into(),
        InitialClaimSettlementError::Capacity(error) => PreparedAdmissionError::from(error).into(),
        InitialClaimSettlementError::ClaimStorage(reason) => {
            PreparedAdmissionError::Unsupported { reason }.into()
        }
        InitialClaimSettlementError::OutcomeRetained {
            outcome,
            lease,
            source,
        } => {
            let claim_ids = claims
                .claims()
                .iter()
                .map(|claim| claim.claim_id())
                .collect::<Vec<_>>();
            drop(lease);
            match outcome {
                JournalInsertOutcome::Committed(receipt) => {
                    PreparedAdmissionRetainedError::Committed {
                        receipt,
                        claim_ids,
                        reason: format!("initial settlement retained capacity: {source}"),
                    }
                    .into()
                }
                JournalInsertOutcome::Failed(failure) => {
                    PreparedAdmissionRetainedError::from_failure_with_reason(
                        failure,
                        claim_ids,
                        format!("initial settlement retained capacity: {source}"),
                    )
                    .into()
                }
            }
        }
    }
}

impl Drop for OwnerRuntime {
    fn drop(&mut self) {
        // Close and wait before ANY original actor resource is released, including unwind.
        // Native-use Drop wakes directly; no actor queue or source/slot mutex is held.
        if self.publication_fences.unresolved() {
            self.resource_admission.invalidate();
        }
        if thread::panicking() {
            self.resource_admission.invalidate();
            if let Some(input) = self.resource_input.as_ref() {
                input
                    .shared
                    .mark_unavailable("original journal runtime is unwinding");
            }
        }
        self.resource_admission.close_and_wait();
        self.native_readers.clear();
        self.native_reader_registry_input.take();
        // Publish loss/cancel original owners before journal authority teardown.
        self.retirement_admission.take();
        drop(self.wal_preparation.take());
        // Original readonly SQLite closes before its pre-lease reservation/profile custody.
        self.legacy_adoption.take();
        // Original pre-start channels/source pin are released before source/profile teardown.
        self.recording_restart_audit.take();
        self.recording_producer.take();
        // Source registrations retain non-Send authority guards and external reserve/volume
        // bindings. Release these maps before the journal/profile core so a global shutdown is
        // visibly conservative and never mistaken for a successful per-source retirement.
        drop(std::mem::take(&mut self.source_volumes));
        // HashMap.clear would retain bucket allocations. Destroy all registry nodes before
        // the original credits that paid them, including during real worker unwind.
        drop(std::mem::take(&mut self.source_registrations));
        drop(std::mem::take(&mut self.retired_source_registrations));
        let mut credit_heap = std::mem::take(&mut self.source_registry_credits).into_iter();
        let current_registry_credits: [Option<current_source_metadata::CurrentInput>;
            current_source_metadata::MAX_CURRENT_CREDITS] =
            std::array::from_fn(|_| credit_heap.next());
        // The bound is checked before every actual retention; free the keeper heap first.
        drop(credit_heap);
        drop(current_registry_credits);
        drop(std::mem::take(&mut self.issued_chunk_tickets));
        self.issued_chunk_ticket_backing.take();
        self.pending_chunk_releases.clear();
        // Keep this order explicit. The reserve root/descriptor must go first, then SQLite and
        // its profile-child lease, then the cross-process volume lock, and finally the profile
        // guard clone retained solely for this runtime.
        #[cfg(test)]
        let observer = self.drop_observer.take();
        #[cfg(test)]
        let probe = self.drop_probe.as_ref();
        self.capacity_provider.take();
        self.reserve.take();
        self.store.take();
        #[cfg(test)]
        if let (Some(observer), Some(probe)) = (observer.as_ref(), probe) {
            let mut observation =
                panic::catch_unwind(AssertUnwindSafe(|| probe.observe())).unwrap_or_default();
            // At this point the store is gone while the volume and final profile capabilities
            // remain held by the runtime.
            observation.profile_available_after_teardown = false;
            observation.volume_available_after_teardown = false;
            record_owner_runtime_drop_observation(observer, observation);
        }
        self.volume_owner.take();
        self.final_profile_guard.take();
        self.resource_input.take();
        #[cfg(test)]
        if let (Some(observer), Some(probe)) = (observer.as_ref(), probe) {
            let mut observation =
                panic::catch_unwind(AssertUnwindSafe(|| probe.observe())).unwrap_or_default();
            observation.profile_held_after_store_drop = false;
            observation.volume_held_after_store_drop = false;
            record_owner_runtime_drop_observation(observer, observation);
        }
    }
}

impl OperationJournalOwner {
    /// Start a worker by consuming an already-open journal store.
    pub fn start(store: OperationJournalStore) -> Result<Self, OperationJournalOwnerStartError> {
        Self::start_inner(
            store,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            false,
            #[cfg(any(test, feature = "test-support"))]
            true,
            #[cfg(any(test, feature = "test-support"))]
            None,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Start an isolated journal fixture without physical capacity authority.
    ///
    /// The real store, profile-child lease, recovery checks and worker are retained, but no
    /// volume owner, protected reserve or live capacity provider is installed. Physical source
    /// authority and capacity admission therefore remain unavailable. This constructor is only
    /// for portable cross-crate tests, never production startup or platform qualification.
    pub fn start_without_capacity_for_test(
        store: OperationJournalStore,
    ) -> Result<Self, OperationJournalOwnerStartError> {
        Self::start_inner(store, None, false, false, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Start an owner with a caller-provided fixture registry for cross-crate lifecycle tests.
    /// This retains the same recovery-only bootstrap and worker ordering as production startup.
    pub fn start_with_capacity_fixture_for_test(
        store: OperationJournalStore,
        registry_path: std::path::PathBuf,
    ) -> Result<Self, OperationJournalOwnerStartError> {
        Self::start_inner(store, None, false, true, Some(registry_path))
    }

    #[cfg(test)]
    fn start_with_command_gate_for_test(
        store: OperationJournalStore,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let owner = Self::start_inner(
            store,
            Some((Arc::clone(&ready), Arc::clone(&release))),
            false,
            false,
            None,
        )
        .expect("operation journal owner start");
        (owner, ready, release)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Start an isolated real worker that panics on its first gated command.
    /// No physical capacity or effect authority is installed by this fixture.
    pub fn start_with_worker_panic_for_test(
        store: OperationJournalStore,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let owner = Self::start_inner(
            store,
            Some((Arc::clone(&ready), Arc::clone(&release))),
            true,
            false,
            None,
        )
        .expect("operation journal owner start");
        (owner, ready, release)
    }

    fn start_inner(
        store: OperationJournalStore,
        #[cfg(any(test, feature = "test-support"))] command_gate: Option<(
            Arc<std::sync::Barrier>,
            Arc<std::sync::Barrier>,
        )>,
        #[cfg(any(test, feature = "test-support"))] panic_on_command: bool,
        #[cfg(any(test, feature = "test-support"))] bootstrap_capacity: bool,
        #[cfg(any(test, feature = "test-support"))] bootstrap_registry_path: Option<
            std::path::PathBuf,
        >,
    ) -> Result<Self, OperationJournalOwnerStartError> {
        let (command_tx, command_rx) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(command_tx));
        #[cfg(test)]
        let prepared_admission_id_allocations = Arc::new(AtomicUsize::new(0));
        #[cfg(test)]
        let synthetic_chunk_write_invocations = Arc::new(AtomicUsize::new(0));
        let panic_shared = Arc::clone(&shared);
        let worker_shared = Arc::clone(&shared);
        #[cfg(test)]
        let worker_prepared_admission_id_allocations =
            Arc::clone(&prepared_admission_id_allocations);
        #[cfg(test)]
        let worker_synthetic_chunk_write_invocations =
            Arc::clone(&synthetic_chunk_write_invocations);
        let worker = thread::Builder::new()
            .name(String::from("yield-operation-journal-owner"))
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_owner(
                        store,
                        command_rx,
                        worker_shared,
                        #[cfg(any(test, feature = "test-support"))]
                        command_gate,
                        #[cfg(any(test, feature = "test-support"))]
                        panic_on_command,
                        #[cfg(any(test, feature = "test-support"))]
                        bootstrap_capacity,
                        #[cfg(any(test, feature = "test-support"))]
                        bootstrap_registry_path,
                        #[cfg(test)]
                        worker_prepared_admission_id_allocations,
                        #[cfg(test)]
                        worker_synthetic_chunk_write_invocations,
                    );
                }));
                if result.is_err() {
                    // An unexpected panic drops the receiver and therefore makes every
                    // outstanding result conservative. Retain a typed status for new callers.
                    panic_shared.mark_unavailable("operation journal owner worker panicked");
                }
            })
            .map_err(OperationJournalOwnerStartError::ThreadSpawn)?;
        Ok(Self {
            shared,
            worker: Some(worker),
            #[cfg(test)]
            prepared_admission_id_allocations,
            #[cfg(all(test, unix))]
            synthetic_chunk_write_invocations,
        })
    }

    /// Return a cloneable client without retaining the journal or profile ownership.
    pub fn client(&self) -> OperationJournalClient {
        OperationJournalClient {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Return the current worker lifecycle without waiting for storage I/O.
    pub fn status(&self) -> OperationJournalOwnerStatus {
        self.shared.status()
    }

    #[cfg(test)]
    fn prepared_admission_id_allocations(&self) -> usize {
        self.prepared_admission_id_allocations
            .load(Ordering::Relaxed)
    }

    #[cfg(all(test, unix))]
    fn synthetic_chunk_write_invocations(&self) -> usize {
        self.synthetic_chunk_write_invocations
            .load(Ordering::Relaxed)
    }

    /// Close new command admission without disconnecting or joining the worker.
    ///
    /// Returns `true` only for the first Initializing/Available-to-Closing transition.
    /// Accepted commands continue draining and the idle worker retains its store/profile
    /// resources until a later [`Self::shutdown`] or Drop disconnects and joins it.
    /// Worker failure remains Unavailable; repeated drain never reopens admission.
    ///
    /// Also closes new counted publication resource uses, without waiting for quiescence.
    /// This does not settle effects or supply clean handoff. Complete publication shutdown
    /// must compose the exact slot/source/charge gates and settlement protocol.
    pub fn begin_command_drain(&self) -> bool {
        matches!(
            self.close_command_admission(),
            OwnerLifecycle::Initializing | OwnerLifecycle::Available
        )
    }

    fn close_command_admission(&self) -> OwnerLifecycle {
        self.shared.resource_admission.close();
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let current = self.shared.lifecycle.load(Ordering::Acquire);
            let current_state = OwnerLifecycle::from_u8(current);
            if matches!(
                current_state,
                OwnerLifecycle::Closing | OwnerLifecycle::Unavailable | OwnerLifecycle::Closed
            ) {
                break current_state;
            }
            #[cfg(test)]
            if let Some((ready, release)) = self
                .shared
                .shutdown_cas_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                ready.wait();
                release.wait();
            }
            match self.shared.lifecycle.compare_exchange(
                current,
                OwnerLifecycle::Closing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break current_state,
                Err(_) => continue,
            }
        }
    }

    /// Stop new admission, drain accepted commands, release the journal, and join the worker.
    ///
    /// Also completes a previously begun command drain. Never call while holding a native
    /// effect, source/control or admission lock needed by worker completion.
    /// Returns `true` only when the worker drained and stopped cleanly. A worker failure returns
    /// `false` and remains visible as [`OperationJournalOwnerStatus::Unavailable`] to retained
    /// clients after the worker resources have been released. Calling shutdown again after a
    /// clean close also returns `false`.
    pub fn shutdown(&mut self) -> bool {
        let previous = self.close_command_admission();
        self.shared
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.shared
                    .mark_unavailable("operation journal owner thread failed during shutdown");
            }
        }
        let final_state = OwnerLifecycle::from_u8(self.shared.lifecycle.load(Ordering::Acquire));
        if final_state != OwnerLifecycle::Unavailable {
            self.shared
                .lifecycle
                .store(OwnerLifecycle::Closed as u8, Ordering::Release);
        }
        previous != OwnerLifecycle::Closed && final_state != OwnerLifecycle::Unavailable
    }
}

impl Drop for OperationJournalOwner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl OperationJournalClient {
    pub(in crate::operation_journal) fn owns_wal_input(&self, input: &InputPermit) -> bool {
        Arc::ptr_eq(&self.shared, &input.shared)
            && input.class == InputClass::Ordinary
            && input.bytes == super::wal_admission::HANDSHAKE_MAX_BYTES
            && !input.publication
    }
    /// Fixed per-owner queue observations. Returns None while accounting is contended.
    /// Backlog ends at accounted dequeue; it is not instantaneous channel length or success.
    pub fn queue_metrics_snapshot(&self) -> Option<JournalQueueMetricsSnapshot> {
        self.shared.queue_metrics.snapshot()
    }

    /// Return the original worker's profile acquisition without waiting or allocating.
    ///
    /// The worker publishes this equality observation after successful bootstrap,
    /// before becoming Available. Clones and retained clients keep that original
    /// identity after closure; a new acquisition cannot redirect an old client.
    /// `None` means bootstrap has not published an identity. This UUID is no
    /// writable capability or admission proof; callers must also check lifecycle
    /// and the owner must revalidate its current guard before admitting work.
    pub fn original_profile_acquisition(&self) -> Option<Uuid> {
        self.shared.original_profile_acquisition.get().copied()
    }

    /// Return the current worker lifecycle without waiting for storage I/O.
    pub fn status(&self) -> OperationJournalOwnerStatus {
        self.shared.status()
    }

    /// Queue one bounded source-authority registration handshake.
    ///
    /// The opaque request carries descriptor-bound evidence and a one-shot source-owner
    /// confirmation. The worker consumes it only after revalidating the retained roots and file
    /// identity; it never receives a source lease or SQLite connection.
    pub fn register_source_authority(
        &self,
        request: SourceAuthorityRegistrationRequest,
    ) -> Result<
        OperationJournalRequest<SourceAuthorityRegistrationReceipt>,
        OperationJournalOwnerQueueError,
    > {
        let input_bytes = current_source_metadata::input_bytes(&request)?;
        self.submit(input_bytes, move |input, result| {
            JournalCommand::RegisterSource {
                request,
                input,
                result,
            }
        })
    }

    /// Authenticate one source-writer-issued metadata envelope on the actual journal worker.
    /// The result supplies custody only; admitted global SQL and physical claims remain separate.
    pub fn authenticate_source_registry_metadata(
        &self,
        candidate: crate::sample_sources::db::SourceRegistryMetadataCandidate,
    ) -> Result<
        OperationJournalRequest<crate::sample_sources::db::SourceRegistryMetadataPermit>,
        OperationJournalOwnerQueueError,
    > {
        self.submit(candidate.input_bytes(), move |input, result| {
            JournalCommand::AuthenticateSourceMetadata {
                candidate,
                input,
                result,
            }
        })
    }

    /// Queue the restrictive source drain transition while retaining its volume resources.
    pub fn begin_source_drain(
        &self,
        receipt: SourceAuthorityRegistrationReceipt,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        self.submit_control(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::BeginSourceDrain {
                receipt,
                input,
                result,
            }
        })
    }

    /// Queue the final source-removal transition after a sealed source-owner retirement ack.
    pub fn finalize_source_retirement(
        &self,
        receipt: SourceAuthorityRegistrationReceipt,
        acknowledgement: SourceAuthorityRetirementAck,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        self.submit_control(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::FinalizeSourceRetirement {
                receipt,
                acknowledgement,
                input,
                result,
            }
        })
    }

    /// Queue one initial intent, moving the bounded record to the worker.
    ///
    /// The codec is traversed before admission so an oversized or otherwise unencodable record
    /// never enters the queue. The worker performs the authoritative store validation again and
    /// returns the store's [`JournalInsertOutcome`] unchanged.
    pub fn persist_intent(
        &self,
        record: StoredOperationRecord,
    ) -> Result<OperationJournalRequest<JournalInsertOutcome>, OperationJournalOwnerQueueError>
    {
        let encoded = bounded_encoded_record(&record)?;
        self.submit(encoded.len(), move |input, result| {
            JournalCommand::PersistIntent {
                encoded,
                input,
                result,
            }
        })
    }

    /// Queue a pre-publication cancellation request.
    pub fn request_cancel_before_publish(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<OperationJournalRequest<JournalCancelRequestReceipt>, OperationJournalOwnerQueueError>
    {
        let encoded = bounded_cancellation_input(observation)?;
        self.submit_control(encoded.len(), move |input, result| JournalCommand::Cancel {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        })
    }

    /// Queue one typed V5 retry observation.
    pub fn record_retry_pending(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<OperationJournalRequest<JournalRetryReceipt>, OperationJournalOwnerQueueError> {
        let encoded = bounded_retry_input(&observation)?;
        self.submit_control(encoded.len(), move |input, result| JournalCommand::Retry {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        })
    }

    /// Persist a capacity-exhaustion retry. The status is projected from durable positive chunk
    /// staging evidence; an Issued claim alone cannot turn RetryPending into PartialNeedsRetry.
    #[allow(dead_code)] // Operation-family producer is deferred by OPT-1565's scope.
    pub(crate) fn record_post_intent_chunk_exhaustion(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<
        OperationJournalRequest<(JournalRetryReceipt, OperationDisposition)>,
        OperationJournalOwnerQueueError,
    > {
        let encoded = bounded_retry_input(&observation)?;
        self.submit_control(encoded.len(), move |input, result| {
            JournalCommand::ChunkExhaustion {
                operation_id,
                expected_sequence,
                encoded,
                input,
                result,
            }
        })
    }

    /// Queue one bounded full-operation admission request.
    ///
    /// The request deliberately remains fail-closed until the descriptor-bound physical
    /// capacity provider is qualified.
    /// A successful queue submission only admits the typed request to the journal owner; it does
    /// not allocate an operation ID, create progress, reserve capacity, or grant dispatch
    /// authority.
    #[allow(dead_code)]
    pub(crate) fn admit_full(
        &self,
        args: UniversalCommandArgs,
        participants: ParticipantSet,
    ) -> Result<OperationJournalRequest<OperationId>, OperationJournalOwnerQueueError> {
        let encoded_len = bounded_admission_input(&args, &participants)?;
        participants.validate_for_new_intent().map_err(|error| {
            OperationJournalOwnerQueueError::InvalidInput {
                reason: error.to_string(),
            }
        })?;
        args.validate_against_participants(&participants)
            .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
                reason: error.to_string(),
            })?;
        self.submit(encoded_len, move |input, result| {
            JournalCommand::AdmitFull {
                args,
                participants,
                input,
                result,
            }
        })
    }

    /// Prepay the complete finite WAL factory/collector envelope on this owner's
    /// ordinary input pool before serialization, validation or topology scratch.
    /// The token is consumed by an original factory and transferred at queueing;
    /// an unused token returns only memory credit. No SQLite or native work runs.
    pub fn reserve_wal_preparation(
        &self,
    ) -> Result<WalPreparationFunding, OperationJournalOwnerQueueError> {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.ensure_available()?;
        Ok(WalPreparationFunding {
            input: super::wal_admission::JournalInput::new(self.shared.reserve_input(
                InputClass::Ordinary,
                super::wal_admission::HANDSHAKE_MAX_BYTES,
            )?),
        })
    }

    /// Collect complete ordered original-owner WAL scope asynchronously on this journal actor.
    /// Queue/Ready observations provide no current registration acknowledgement, decision latch,
    /// capacity grant or durable intent. Abort/Drop requests original-owner cleanup only.
    pub fn prepare_wal_owners(
        &self,
        mut input: WalPreparationInput,
    ) -> Result<OperationJournalRequest<WalPreparationHandle>, OperationJournalOwnerQueueError>
    {
        input
            .retained_bytes()
            .ok_or_else(|| OperationJournalOwnerQueueError::InvalidInput {
                reason: "bounded original WAL preparation input".into(),
            })?;
        // The original factory already prepaid the full envelope. Never debit a
        // second pool or release/reacquire between factory and collector.
        if !input
            .funding
            .as_ref()
            .expect("funded WAL factory")
            .input
            .belongs_to(self)
        {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: "WAL preparation belongs to another journal owner".into(),
            });
        }
        let debit = input.funding.take().expect("sole WAL factory debit").input;
        self.submit_prepaid(InputClass::Ordinary, debit, move |debit, result| {
            JournalCommand::PrepareWalOwners {
                preparation: input,
                input: debit,
                result,
            }
        })
    }

    /// Queue one common prepared-admission request. The input is validated before queue
    /// admission and again on the owner thread; existing family callers remain on `admit_full`
    /// until their authority adapters are wired by a later milestone.
    #[allow(dead_code)]
    pub(crate) fn admit_prepared(
        &self,
        prepared: PreparedAdmissionInput,
    ) -> Result<OperationJournalRequest<OperationId>, OperationJournalOwnerQueueError> {
        prepared.validate_shape().map_err(|error| {
            OperationJournalOwnerQueueError::InvalidInput {
                reason: error.to_string(),
            }
        })?;
        let input_bytes = bounded_prepared_admission_input(&prepared)?;
        let historical = !matches!(
            prepared.args,
            UniversalCommandArgs::SourceRegistryRetirement(_)
        );
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            input_bytes,
            false,
            historical,
            move |input, result| JournalCommand::AdmitPrepared {
                prepared,
                input,
                result,
            },
        )
    }

    /// Queue one bounded post-intent chunk claim. The owner allocates no chunk or operation
    /// identity here; the supplied stable identity and immutable request are validated again on
    /// the owner thread before one provisional lease is persisted with the ordinary claim row.
    #[allow(dead_code)]
    pub(crate) fn claim_chunk(
        &self,
        request: ChunkClaimRequest,
    ) -> Result<OperationJournalRequest<FundedChunkClaimRecord>, OperationJournalOwnerQueueError>
    {
        request
            .validate()
            .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
                reason: error.to_string(),
            })?;
        let input_bytes = std::mem::size_of::<ChunkClaimRequest>()
            .checked_add(request.output_key().len())
            .ok_or(OperationJournalOwnerQueueError::RecordTooLarge {
                actual: usize::MAX,
                maximum: OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES,
            })?;
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            input_bytes,
            false,
            true,
            move |input, result| JournalCommand::ClaimChunk {
                request,
                input,
                result,
            },
        )
    }

    /// Queue the one-shot Claimed-to-Issued transition. An already-issued row is never reissued;
    /// a dropped permit remains an ambiguous durable charge until explicit reconciliation.
    #[allow(dead_code)]
    pub(crate) fn issue_chunk(
        &self,
        chunk_id: ChunkId,
        operation_id: OperationId,
    ) -> Result<OperationJournalRequest<ChunkWritePermit>, OperationJournalOwnerQueueError> {
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            std::mem::size_of::<JournalCommand>(),
            false,
            true,
            move |input, result| JournalCommand::IssueChunk {
                chunk_id,
                operation_id,
                input,
                result,
            },
        )
    }

    /// Issue a sealed effect-scoped grant from a live registered source and a durable chunk
    /// claim. The command carries observations, but only the worker's retained registration,
    /// source pin, capacity provider, and Claimed-to-Issued transition construct the grant.
    #[allow(dead_code)] // Consumed by the file-owner adapter in the next OPT-1857 stage.
    pub(crate) fn issue_live_chunk_effect(
        &self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        spec: LiveChunkEffectSpec,
    ) -> Result<OperationJournalRequest<LiveChunkEffectGrant>, OperationJournalOwnerQueueError>
    {
        let input_bytes =
            spec.queued_bytes()
                .ok_or(OperationJournalOwnerQueueError::RecordTooLarge {
                    actual: usize::MAX,
                    maximum: OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES,
                })?;
        self.submit(input_bytes, move |input, result| {
            JournalCommand::IssueLiveChunk {
                chunk_id,
                operation_id,
                registration,
                source_pin,
                spec,
                input,
                result,
            }
        })
    }

    /// Queue a file-owner staging result for durable CopyValidated acknowledgement and one
    /// one-shot publication grant. The worker rechecks the registered source and its live pin.
    #[allow(dead_code)] // First consumer is the publication route following this checkpoint.
    pub(crate) fn checkpoint_and_grant_staging(
        &self,
        target: PreparedPublicationTarget,
        charge_selector: publication_charge::Selector,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
    ) -> Result<OperationJournalRequest<StagingPublicationGrant>, OperationJournalOwnerQueueError>
    {
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            64 * 1024,
            true,
            true,
            move |input, result| JournalCommand::CheckpointStaging {
                target,
                charge_selector,
                registration,
                source_pin,
                input,
                result,
            },
        )
    }

    /// Persist a positive incomplete chunk checkpoint from the exact issued ticket, live source
    /// pin, and a file-owner validation result. The returned receipt carries no effect authority.
    #[allow(dead_code)] // Ordinary-family producer is deferred by OPT-1856's scope.
    pub(crate) fn checkpoint_live_chunk_staging(
        &self,
        chunk_id: ChunkId,
        issue_id: Uuid,
        validation: FileOperationResult,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
    ) -> Result<OperationJournalRequest<ChunkStagingReceipt>, OperationJournalOwnerQueueError> {
        self.submit(64 * 1024, move |input, result| {
            JournalCommand::CheckpointLiveChunkStaging {
                chunk_id,
                issue_id,
                validation,
                registration,
                source_pin,
                input,
                result,
            }
        })
    }

    /// Queue one bounded synthetic write entry. The ticket is checked against the live owner
    /// registry and fresh descriptor/provider facts on the worker; it never carries execution or
    /// filesystem authority across this queue boundary.
    #[allow(dead_code)]
    pub(crate) fn begin_chunk_write(
        &self,
        permit: ChunkWritePermit,
        identity: ChunkWriteIdentity,
        logical_bytes: u64,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        if logical_bytes == 0 {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("chunk write byte budget is zero"),
            });
        }
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::BeginChunkWrite {
                permit,
                identity,
                logical_bytes,
                input,
                result,
            }
        })
    }

    /// Queue logical release of a claimed or issued chunk. The permit is consumed by the owner
    /// command; durable release is committed before provider accounting is decremented.
    #[allow(dead_code)]
    pub(crate) fn release_chunk(
        &self,
        permit: ChunkWritePermit,
        release_id: Uuid,
    ) -> Result<OperationJournalRequest<OrdinaryClaimReleaseReceipt>, OperationJournalOwnerQueueError>
    {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("chunk release identity is nil"),
            });
        }
        self.submit_control(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::ReleaseChunk {
                permit,
                release_id,
                input,
                result,
            }
        })
    }

    /// Queue retry of an already durable chunk release after a provider or acknowledgement
    /// failure. The exact release identity is required; no new charge or receipt is minted.
    #[allow(dead_code)]
    pub(crate) fn retry_chunk_release(
        &self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
    ) -> Result<OperationJournalRequest<OrdinaryClaimReleaseReceipt>, OperationJournalOwnerQueueError>
    {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("chunk release identity is nil"),
            });
        }
        self.submit_control(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::RetryChunkRelease {
                chunk_id,
                operation_id,
                release_id,
                input,
                result,
            }
        })
    }

    /// Queue cancellation of a still-Claimed chunk. An Issued permit must instead use the exact
    /// sealed permit release path, because a permit may already have escaped to its consumer.
    #[allow(dead_code)]
    pub(crate) fn cancel_chunk(
        &self,
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
    ) -> Result<OperationJournalRequest<OrdinaryClaimReleaseReceipt>, OperationJournalOwnerQueueError>
    {
        if release_id.is_nil() {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("chunk release identity is nil"),
            });
        }
        self.submit_control(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::CancelChunk {
                chunk_id,
                operation_id,
                release_id,
                input,
                result,
            }
        })
    }

    #[cfg(all(test, target_os = "macos"))]
    fn inject_active_recovery_charge_for_test(
        &self,
        operation_id: OperationId,
        volume: VolumeIdentity,
        budget_bytes: u64,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::InjectActiveRecoveryCharge {
                operation_id,
                volume,
                budget_bytes,
                input,
                result,
            }
        })
    }

    #[cfg(all(test, target_os = "macos"))]
    fn inspect_capacity_for_test(
        &self,
    ) -> Result<OperationJournalRequest<CapacityLedgerTestSnapshot>, OperationJournalOwnerQueueError>
    {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::InspectCapacity { input, result }
        })
    }

    /// Queue one bounded point lookup.
    pub fn get(
        &self,
        operation_id: OperationId,
    ) -> Result<
        OperationJournalRequest<Option<StoredOperationRecord>>,
        OperationJournalOwnerQueueError,
    > {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::Get {
                operation_id,
                input,
                result,
            }
        })
    }

    /// Queue one bounded recovery page after an optional observation cursor.
    pub fn scan_recovery_page(
        &self,
        cursor: Option<RecoveryCursor>,
    ) -> Result<OperationJournalRequest<RecoveryPage>, OperationJournalOwnerQueueError> {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::Recovery {
                cursor,
                input,
                result,
            }
        })
    }

    /// Queue one bounded compact status page over both journal families.
    pub fn scan_recovery_status_page(
        &self,
        cursor: Option<RecoveryCursor>,
    ) -> Result<OperationJournalRequest<super::RecoveryStatusPage>, OperationJournalOwnerQueueError>
    {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::RecoveryStatus {
                cursor,
                input,
                result,
            }
        })
    }

    /// Queue one crate-internal bounded full-progress point lookup.
    #[allow(dead_code)]
    pub(crate) fn get_full(
        &self,
        operation_id: OperationId,
    ) -> Result<
        OperationJournalRequest<Option<StoredFullProgressRecord>>,
        OperationJournalOwnerQueueError,
    > {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::GetFull {
                operation_id,
                input,
                result,
            }
        })
    }

    /// Queue one crate-internal full-progress observation write.
    ///
    /// The record is bounded and validated before it enters the owner queue. The worker decodes
    /// the same bytes and delegates the expected-sequence CAS to the profile-owned store; this
    /// command persists an observation only and never reconstructs execution authority.
    #[allow(dead_code)]
    pub(crate) fn persist_full(
        &self,
        record: StoredFullProgressRecord,
        expected_sequence: Option<i64>,
    ) -> Result<OperationJournalRequest<FullProgressWriteOutcome>, OperationJournalOwnerQueueError>
    {
        let encoded = bounded_full_progress_record(&record)?;
        self.submit(encoded.len(), move |input, result| {
            JournalCommand::PersistFull {
                expected_sequence,
                encoded,
                input,
                result,
            }
        })
    }

    /// Queue one crate-internal combined legacy/full recovery page.
    #[allow(dead_code)]
    pub(crate) fn scan_coordinator_recovery_page(
        &self,
        cursor: Option<RecoveryCursor>,
    ) -> Result<OperationJournalRequest<CoordinatorRecoveryPage>, OperationJournalOwnerQueueError>
    {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::CoordinatorRecovery {
                cursor,
                input,
                result,
            }
        })
    }

    /// Retain exact captured bytes under current source ownership and an admitted chunk claim.
    /// The returned receipt proves storage only, not a live write grant or physical completion.
    #[allow(dead_code)] // Recording producer integration is OPT-2058.
    pub(crate) fn retain_recording_chunk(
        &self,
        chunk: RecordingCapturedChunk,
        proposal: &RecordingProposal,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
    ) -> Result<OperationJournalRequest<RecordingChunkReceipt>, OperationJournalOwnerQueueError>
    {
        let encoded =
            chunk
                .encode(proposal)
                .map_err(|e| OperationJournalOwnerQueueError::InvalidRecord {
                    reason: e.to_string(),
                })?;
        self.submit(encoded.len(), move |input, result| {
            JournalCommand::RetainRecording {
                operation_id: chunk.operation_id(),
                encoded,
                registration,
                source_pin,
                input,
                result,
            }
        })
    }
    /// Recover a bounded page of actual captured bytes, never an execution grant.
    #[allow(dead_code)] // Recording recovery consumer is OPT-2058.
    pub(crate) fn recording_chunk_page(
        &self,
        operation_id: OperationId,
        after_sequence: Option<i64>,
    ) -> Result<OperationJournalRequest<RecordingChunkPage>, OperationJournalOwnerQueueError> {
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::RecordingRecovery {
                operation_id,
                after_sequence,
                input,
                result,
            }
        })
    }

    /// Recompute retained Frozen bytes; the result grants no execution permission.
    #[allow(dead_code)] // Recovery consumer integration remains separate from this observation.
    pub(crate) fn audit_recording_frozen_prefix(
        &self,
        operation_id: OperationId,
    ) -> Result<
        OperationJournalRequest<super::recording_commands::prefix::RecordingPrefixSeal>,
        OperationJournalOwnerQueueError,
    > {
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            CONTROL_INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::RecordingRestartAudit {
                operation_id,
                input,
                result,
            },
        )
    }

    fn submit<T, F>(
        &self,
        input_bytes: usize,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(InputPermit, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        self.submit_class(InputClass::Ordinary, input_bytes, build)
    }

    fn submit_control<T, F>(
        &self,
        input_bytes: usize,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(InputPermit, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        self.submit_class(InputClass::Control, input_bytes, build)
    }

    fn submit_class<T, F>(
        &self,
        class: InputClass,
        input_bytes: usize,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(InputPermit, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        self.submit_funded(class, input_bytes, false, build)
    }

    fn submit_funded<T, F>(
        &self,
        class: InputClass,
        input_bytes: usize,
        publication: bool,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(InputPermit, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        self.submit_funded_with_reply(class, input_bytes, publication, false, build)
    }

    fn submit_funded_with_reply<T, F>(
        &self,
        class: InputClass,
        input_bytes: usize,
        publication: bool,
        historical: bool,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(InputPermit, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.ensure_available()?;
        let sender = self
            .shared
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let mut input = if publication {
            self.shared.reserve_publication_input(input_bytes)?
        } else {
            self.shared.reserve_input(class, input_bytes)?
        };
        let historical_reply = if historical {
            // Fund the Arc allocation (including both reference counters) before allocating.
            let header =
                self.shared
                    .reserve_input_kind(class, HISTORICAL_REPLY_BYTES, publication)?;
            let backing = HistoricalReply::new(HistoricalReplyBacking {
                _header: header,
                consumer: Mutex::new(None),
                observer: Mutex::new(None),
                namespace: Mutex::new(None),
                control: Mutex::new(None),
                #[cfg(test)]
                consumer_refused: std::sync::atomic::AtomicBool::new(false),
            });
            input.historical_reply = Some(backing.clone());
            Some(backing)
        } else {
            None
        };
        self.enqueue_with_input(class, input, historical_reply, sender, build)
    }

    fn submit_prepaid<T, F>(
        &self,
        class: InputClass,
        input: super::wal_admission::JournalInput,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(
            super::wal_admission::JournalInput,
            SyncSender<Result<T, OperationJournalOwnerError>>,
        ) -> JournalCommand,
    {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.ensure_available()?;
        let sender = self
            .shared
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        self.enqueue_with_input(class, input, None, sender, build)
    }

    fn enqueue_with_input<T, F, C>(
        &self,
        class: InputClass,
        input: C,
        historical_reply: Option<HistoricalReply>,
        sender: OwnerCommandSenders,
        build: F,
    ) -> Result<OperationJournalRequest<T>, OperationJournalOwnerQueueError>
    where
        F: FnOnce(C, SyncSender<Result<T, OperationJournalOwnerError>>) -> JournalCommand,
    {
        let result_permit = self.shared.reserve_result(class)?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let command = build(input, result_tx);
        match sender.sender(class).try_send(command) {
            Ok(()) => Ok(OperationJournalRequest {
                receiver: Some(result_rx),
                historical_reply,
                result_permit: Some(result_permit),
            }),
            Err(TrySendError::Full(command)) => {
                drop(command);
                drop(result_rx);
                drop(result_permit);
                Err(OperationJournalOwnerQueueError::Full)
            }
            Err(TrySendError::Disconnected(command)) => {
                drop(command);
                drop(result_rx);
                drop(result_permit);
                self.shared
                    .mark_unavailable("operation journal owner disconnected");
                Err(OperationJournalOwnerQueueError::Closed)
            }
        }
    }
}

fn bounded_encoded_record(
    record: &StoredOperationRecord,
) -> Result<Vec<u8>, OperationJournalOwnerQueueError> {
    match encode(record) {
        Ok(encoded) => Ok(encoded),
        Err(EncodeError::Oversize { actual, maximum }) => {
            Err(OperationJournalOwnerQueueError::RecordTooLarge { actual, maximum })
        }
        Err(error) => Err(OperationJournalOwnerQueueError::InvalidRecord {
            reason: error.to_string(),
        }),
    }
}

fn bounded_full_progress_record(
    record: &StoredFullProgressRecord,
) -> Result<Vec<u8>, OperationJournalOwnerQueueError> {
    match encode_full_progress(record) {
        Ok(encoded) => Ok(encoded),
        Err(FullProgressCodecError::OutputOversize { actual, maximum }) => {
            Err(OperationJournalOwnerQueueError::RecordTooLarge { actual, maximum })
        }
        Err(error) => Err(OperationJournalOwnerQueueError::InvalidRecord {
            reason: error.to_string(),
        }),
    }
}

fn bounded_cancellation_input(
    observation: CancellationRequestObservation,
) -> Result<Vec<u8>, OperationJournalOwnerQueueError> {
    observation
        .validate()
        .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
            reason: error.to_string(),
        })?;
    bounded_json_input(&observation)
}

fn bounded_retry_input(
    observation: &RetryObservation,
) -> Result<Vec<u8>, OperationJournalOwnerQueueError> {
    observation
        .validate()
        .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
            reason: error.to_string(),
        })?;
    bounded_json_input(observation)
}

#[allow(dead_code)]
fn bounded_admission_input(
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
) -> Result<usize, OperationJournalOwnerQueueError> {
    let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
    match serde_json::to_writer(&mut writer, &(args, participants)) {
        Ok(()) => Ok(writer.finish().len()),
        Err(_error) if writer.exceeded() => Err(OperationJournalOwnerQueueError::RecordTooLarge {
            actual: writer.attempted(),
            maximum: writer.limit(),
        }),
        Err(error) => Err(OperationJournalOwnerQueueError::InvalidInput {
            reason: error.to_string(),
        }),
    }
}

#[allow(dead_code)]
fn bounded_prepared_admission_input(
    prepared: &PreparedAdmissionInput,
) -> Result<usize, OperationJournalOwnerQueueError> {
    let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
    match serde_json::to_writer(
        &mut writer,
        &(
            &prepared.intent,
            &prepared.universal_intent,
            &prepared.args,
            &prepared.participants,
            &prepared.plan,
            &prepared.claims,
            prepared.timestamp_unix_ms,
        ),
    ) {
        Ok(()) => Ok(writer.finish().len()),
        Err(_error) if writer.exceeded() => Err(OperationJournalOwnerQueueError::RecordTooLarge {
            actual: writer.attempted(),
            maximum: writer.limit(),
        }),
        Err(error) => Err(OperationJournalOwnerQueueError::InvalidInput {
            reason: error.to_string(),
        }),
    }
}

fn bounded_json_input<T: serde::Serialize>(
    value: &T,
) -> Result<Vec<u8>, OperationJournalOwnerQueueError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        OperationJournalOwnerQueueError::InvalidInput {
            reason: error.to_string(),
        }
    })?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(OperationJournalOwnerQueueError::RecordTooLarge {
            actual: encoded.len(),
            maximum: MAX_RECORD_BYTES,
        });
    }
    Ok(encoded)
}

struct OwnerShared {
    original_profile_acquisition: OnceLock<Uuid>,
    queue_metrics: Arc<QueueMetrics>,
    resource_admission: Arc<ResourceAdmission>,
    commands: Mutex<Option<OwnerCommandSenders>>,
    admission_gate: Mutex<()>,
    lifecycle: Arc<AtomicU8>,
    unavailable: Mutex<Option<String>>,
    unavailable_startup_credit: Mutex<Option<StartupCredit>>,
    input_bytes: Arc<Mutex<InputBudget>>,
    #[cfg(test)]
    historical_scratch_maximum: AtomicUsize,
    #[cfg(test)]
    recovery_audit_pause: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    recovery_audit_entered: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    historical_consumer_pause: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    historical_consumer_entered: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    provider_reconciliation_component: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    startup_component: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    startup_component_claims: AtomicUsize,
    results: Mutex<ResultBudget>,
    #[cfg(test)]
    shutdown_cas_gate: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}

impl OwnerShared {
    fn new(commands: OwnerCommandSenders) -> Self {
        // Debit all original shared startup gate/pool allocations before allocating.
        let initial = InputBudget::shared_initial();
        let input_bytes = Arc::new(Mutex::new(initial));
        let lifecycle = Arc::new(AtomicU8::new(OwnerLifecycle::Initializing as u8));
        let resource_admission = ResourceAdmission::new();
        Self {
            original_profile_acquisition: OnceLock::new(),
            queue_metrics: Arc::clone(&commands.ordinary.metrics),
            resource_admission,
            commands: Mutex::new(Some(commands)),
            admission_gate: Mutex::new(()),
            lifecycle,
            unavailable: Mutex::new(None),
            unavailable_startup_credit: Mutex::new(None),
            input_bytes,
            #[cfg(test)]
            historical_scratch_maximum: AtomicUsize::new(0),
            #[cfg(test)]
            recovery_audit_pause: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            recovery_audit_entered: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            historical_consumer_pause: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            historical_consumer_entered: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            provider_reconciliation_component: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            startup_component: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            startup_component_claims: AtomicUsize::new(usize::MAX),
            results: Mutex::new(ResultBudget::new()),
            #[cfg(test)]
            shutdown_cas_gate: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_shutdown_cas_gate_for_test(&self) -> (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *self
            .shutdown_cas_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((Arc::clone(&ready), Arc::clone(&release)));
        (ready, release)
    }

    fn status(&self) -> OperationJournalOwnerStatus {
        match OwnerLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire)) {
            OwnerLifecycle::Initializing => OperationJournalOwnerStatus::Initializing,
            OwnerLifecycle::Available => OperationJournalOwnerStatus::Available,
            OwnerLifecycle::Unavailable => OperationJournalOwnerStatus::Unavailable {
                reason: self
                    .unavailable
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .unwrap_or_else(|| String::from("operation journal owner unavailable")),
            },
            OwnerLifecycle::Closing => OperationJournalOwnerStatus::Closing,
            OwnerLifecycle::Closed => OperationJournalOwnerStatus::Closed,
        }
    }

    fn ensure_available(&self) -> Result<(), OperationJournalOwnerQueueError> {
        match OwnerLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire)) {
            OwnerLifecycle::Initializing => Err(OperationJournalOwnerQueueError::Initializing),
            OwnerLifecycle::Available => Ok(()),
            OwnerLifecycle::Unavailable => Err(OperationJournalOwnerQueueError::Unavailable {
                reason: self
                    .unavailable
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .unwrap_or_else(|| String::from("operation journal owner unavailable")),
            }),
            OwnerLifecycle::Closing | OwnerLifecycle::Closed => {
                Err(OperationJournalOwnerQueueError::Closed)
            }
        }
    }

    fn reserve_input(
        self: &Arc<Self>,
        class: InputClass,
        requested: usize,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        self.reserve_input_kind(class, requested, false)
    }

    /// Original pool debit with an additional publication ceiling, not another allowance.
    /// Only fixed private producer callsites select this class; callers cannot borrow control.
    fn reserve_publication_input(
        self: &Arc<Self>,
        requested: usize,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        self.reserve_input_kind(InputClass::Ordinary, requested, true)
    }

    fn reserve_input_kind(
        self: &Arc<Self>,
        class: InputClass,
        requested: usize,
        publication: bool,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        if requested == 0 || (publication && class != InputClass::Ordinary) {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: String::from("journal input credit must be nonzero"),
            });
        }
        let mut budget = self.input_bytes.lock().map_err(|_| {
            self.mark_unavailable("journal input credit mutex is poisoned");
            OperationJournalOwnerQueueError::Unavailable {
                reason: "journal input credit mutex is poisoned".into(),
            }
        })?;
        if !budget.valid() {
            budget.unavailable = true;
            drop(budget);
            self.mark_unavailable("journal input credit accounting is unavailable");
            return Err(OperationJournalOwnerQueueError::Unavailable {
                reason: "journal input credit accounting is unavailable".into(),
            });
        }
        let publication_after = if publication {
            Some(budget.publication_remaining.checked_sub(requested).ok_or(
                OperationJournalOwnerQueueError::InputBytesFull {
                    limit: PUBLICATION_INPUT_BYTES,
                    requested,
                },
            )?)
        } else {
            None
        };
        let after = budget.remaining[class.index()]
            .checked_sub(requested)
            .ok_or(OperationJournalOwnerQueueError::InputBytesFull {
                limit: class.limit(),
                requested,
            })?;
        // Both checked debits succeed before either original counter changes.
        budget.remaining[class.index()] = after;
        if let Some(after) = publication_after {
            budget.publication_remaining = after;
        }
        Ok(InputPermit {
            shared: Arc::clone(self),
            bytes: requested,
            class,
            publication,
            historical_reply: None,
            refund: true,
        })
    }

    fn reserve_result(
        self: &Arc<Self>,
        class: InputClass,
    ) -> Result<ResultPermit, OperationJournalOwnerQueueError> {
        let mut budget = self.results.lock().map_err(|_| {
            self.mark_unavailable("journal result credit mutex is poisoned");
            OperationJournalOwnerQueueError::Unavailable {
                reason: "journal result credit mutex is poisoned".into(),
            }
        })?;
        if !budget.valid() {
            budget.unavailable = true;
            drop(budget);
            self.mark_unavailable("journal result credit accounting is unavailable");
            return Err(OperationJournalOwnerQueueError::Unavailable {
                reason: "journal result credit accounting is unavailable".into(),
            });
        }
        let remaining = &mut budget.remaining[class.index()];
        *remaining = remaining
            .checked_sub(1)
            .ok_or(OperationJournalOwnerQueueError::ResultsFull)?;
        Ok(ResultPermit {
            shared: Arc::clone(self),
            class,
        })
    }

    fn release_input(&self, class: InputClass, bytes: usize, publication: bool) {
        let Ok(mut budget) = self.input_bytes.lock() else {
            self.mark_unavailable("journal input credit mutex is poisoned");
            return;
        };
        let mut restored = *budget;
        let ordinary_after = budget.remaining[class.index()]
            .checked_add(bytes)
            .filter(|n| *n <= class.limit());
        let publication_after = if publication {
            budget
                .publication_remaining
                .checked_add(bytes)
                .filter(|n| *n <= PUBLICATION_INPUT_BYTES)
        } else {
            Some(budget.publication_remaining)
        };
        if let (Some(ordinary), Some(publication_bytes)) = (ordinary_after, publication_after) {
            restored.remaining[class.index()] = ordinary;
            restored.publication_remaining = publication_bytes;
        }
        if !budget.valid()
            || bytes == 0
            || ordinary_after.is_none()
            || publication_after.is_none()
            || (publication && class != InputClass::Ordinary)
            || !restored.valid()
        {
            budget.unavailable = true;
            drop(budget);
            self.mark_unavailable("journal input credit refund cannot be confirmed");
            return;
        }
        // Last backing-holder Drop returns memory credits together; no physical claim changes.
        *budget = restored;
    }

    fn release_result(&self, class: InputClass) {
        let Ok(mut budget) = self.results.lock() else {
            self.mark_unavailable("journal result credit mutex is poisoned");
            return;
        };
        let after = budget.remaining[class.index()]
            .checked_add(1)
            .filter(|n| *n <= class.result_limit());
        if !budget.valid() || after.is_none() {
            budget.unavailable = true;
            drop(budget);
            self.mark_unavailable("journal result credit refund cannot be confirmed");
            return;
        }
        budget.remaining[class.index()] = after.expect("checked finite result refund");
    }

    fn mark_unavailable(&self, reason: &str) {
        self.resource_admission.invalidate();
        *self
            .unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(String::from(reason));
        self.lifecycle
            .store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
    }
}

/// Sole original journal memory debit for a WAL preparation factory. It is neither
/// physical capacity nor permission to prepare owners or persist an intent.
/// Obtain it with [`OperationJournalClient::reserve_wal_preparation`].
/// ```compile_fail
/// use yield_library::operation_journal::owner::WalPreparationFunding;
/// let invented = WalPreparationFunding {};
/// ```
/// ```compile_fail
/// fn copy(input: &yield_library::operation_journal::owner::WalPreparationFunding) {
///     let _ = input.clone();
///     let _: yield_library::operation_journal::owner::WalPreparationFunding = input.clone();
/// }
/// ```
pub struct WalPreparationFunding {
    pub(in crate::operation_journal) input: super::wal_admission::JournalInput,
}

pub(super) struct InputPermit {
    refund: bool,
    shared: Arc<OwnerShared>,
    bytes: usize,
    class: InputClass,
    publication: bool,
    historical_reply: Option<HistoricalReply>,
}

struct HistoricalReplyBacking {
    consumer: Mutex<Option<InputPermit>>,
    observer: Mutex<Option<HistoricalObserverCredits>>,
    namespace: Mutex<Option<namespace_credit::NamespaceCredits>>,
    control: Mutex<Option<control_credit::ControlCredits>>,
    #[cfg(test)]
    consumer_refused: std::sync::atomic::AtomicBool,
    // Release working/consumer storage first; the private wrapper already freed the Arc.
    _header: InputPermit,
}
// This first-stage refusal can occur before any observer/consumer debit succeeds.
// Fund its owned reason (including String growth) BEFORE queueing/Arc allocation.
const HISTORICAL_REFUSAL_REPLY_BYTES: usize =
    2 * "historical claim observation resource envelope exceeded".len();
const HISTORICAL_REPLY_BYTES: usize = std::mem::size_of::<HistoricalReplyBacking>()
    + 2 * std::mem::size_of::<usize>()
    + HISTORICAL_REFUSAL_REPLY_BYTES;

struct HistoricalObserverCredits {
    _sql: InputPermit,
    _parser: Option<InputPermit>,
    _envelope: Option<InputPermit>,
    _sqlite_error: Option<InputPermit>,
}

impl InputPermit {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn historical_test_input(bytes: usize) -> Self {
        let (senders, _receivers) = OwnerCommandReceivers::channel();
        Arc::new(OwnerShared::new(senders))
            .reserve_input(InputClass::Ordinary, bytes)
            .expect("test original input pool")
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn historical_test_namespace_input(bytes: usize) -> Self {
        let (senders, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(senders));
        let header = shared
            .reserve_input(InputClass::Ordinary, HISTORICAL_REPLY_BYTES)
            .expect("original namespace reply header");
        let mut input = shared
            .reserve_input(InputClass::Ordinary, bytes)
            .expect("original namespace test input");
        input.historical_reply = Some(HistoricalReply::new(HistoricalReplyBacking {
            consumer: Mutex::new(None),
            observer: Mutex::new(None),
            namespace: Mutex::new(None),
            control: Mutex::new(None),
            #[cfg(test)]
            consumer_refused: std::sync::atomic::AtomicBool::new(false),
            _header: header,
        }));
        input
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn historical_test_funded_bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(super) fn historical_test_ordinary_limit() -> usize {
        InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
    }

    #[cfg(test)]
    pub(super) fn historical_test_remaining(&self) -> usize {
        self.shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()]
    }

    #[cfg(test)]
    pub(super) fn recovery_audit_test_gate(&self) {
        if self.shared.recovery_audit_pause.load(Ordering::Acquire) {
            self.shared
                .recovery_audit_entered
                .store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.shared.recovery_audit_pause.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "original recovery audit test gate timed out"
                );
                std::thread::yield_now();
            }
        }
    }

    pub(in crate::operation_journal) fn covers_original_pool(
        &self,
        input: &Self,
        bytes: usize,
    ) -> bool {
        self.bytes >= bytes
            && self.class == input.class
            && self.publication == input.publication
            && Arc::ptr_eq(&self.shared.input_bytes, &input.shared.input_bytes)
    }

    pub(super) fn has_historical_error_consumer(&self) -> bool {
        self.historical_reply.is_some()
    }

    /// Stop only after the same original namespace has succeeded, before the
    /// original consumer loan. Tests can then apply pressure at that exact stage.
    #[cfg(test)]
    fn historical_consumer_test_gate(&self) {
        if self
            .shared
            .historical_consumer_pause
            .load(Ordering::Acquire)
        {
            self.shared
                .historical_consumer_entered
                .store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self
                .shared
                .historical_consumer_pause
                .load(Ordering::Acquire)
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "original historical consumer test gate timed out"
                );
                std::thread::yield_now();
            }
        }
    }

    pub(super) fn retain_historical_error(
        &self,
        mut sql: InputPermit,
        parser: Option<InputPermit>,
        envelope: Option<InputPermit>,
        sqlite_error: Option<InputPermit>,
    ) {
        if let Some(backing) = &self.historical_reply {
            // Each named allocation is funded on the original pool. Terminal permits
            // contain no reply Arc; the retained backing cannot form a cycle.
            let mut selected = backing.observer.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(previous) = selected.take() {
                // Nested original producers can contribute distinct native and
                // record loans to the same failure. Transfer all original debits
                // without allocating a chain or replacing/refunding earlier loans.
                sql.absorb_historical_scratch(previous._sql);
                for credit in [previous._parser, previous._envelope, previous._sqlite_error]
                    .into_iter()
                    .flatten()
                {
                    sql.absorb_historical_scratch(credit);
                }
            }
            *selected = Some(HistoricalObserverCredits {
                _sql: sql,
                _parser: parser,
                _envelope: envelope,
                _sqlite_error: sqlite_error,
            });
        }
    }

    pub(super) fn absorb_historical_scratch(&mut self, mut other: InputPermit) {
        // Both values are privately reserved terminal scratch from this exact
        // ordinary pool and matching publication subset. No new allowance or authority.
        assert!(Arc::ptr_eq(&self.shared, &other.shared));
        assert!(self.class == InputClass::Ordinary && other.class == self.class);
        assert!(self.publication == other.publication && self.refund && other.refund);
        assert!(self.historical_reply.is_none() && other.historical_reply.is_none());
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .expect("existing original pool total");
        other.refund = false;
    }

    pub(super) fn historical_error_consumed(&self) {
        if let Some(backing) = &self.historical_reply {
            drop(
                backing
                    .observer
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take(),
            );
        }
    }

    fn fund_historical_consumer(&self) -> Result<(), OperationJournalStoreError> {
        if let Some(backing) = &self.historical_reply {
            let permit =
                match self.reserve_historical_scratch(super::store::HISTORICAL_DIAGNOSTIC_BYTES) {
                    Ok(permit) => permit,
                    Err(error) => {
                        #[cfg(test)]
                        backing.consumer_refused.store(true, Ordering::Release);
                        return Err(error);
                    }
                };
            *backing.consumer.lock().unwrap_or_else(|p| p.into_inner()) = Some(permit);
        }
        Ok(())
    }

    /// Private scratch debit from this exact command's original pool and publication subset.
    pub(super) fn reserve_historical_scratch(
        &self,
        bytes: usize,
    ) -> Result<Self, OperationJournalStoreError> {
        if self.class != InputClass::Ordinary {
            return Err(OperationJournalStoreError::HistoricalClaimResourceBound);
        }
        let permit = self
            .shared
            .reserve_input_kind(InputClass::Ordinary, bytes, self.publication)
            .map_err(|_| OperationJournalStoreError::HistoricalClaimResourceBound)?;
        #[cfg(test)]
        self.shared
            .historical_scratch_maximum
            .fetch_max(bytes, Ordering::Relaxed);
        Ok(permit)
    }
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        if self.refund {
            self.shared
                .release_input(self.class, self.bytes, self.publication);
        }
    }
}

struct ResultPermit {
    shared: Arc<OwnerShared>,
    class: InputClass,
}

impl Drop for ResultPermit {
    fn drop(&mut self) {
        self.shared.release_result(self.class);
    }
}

/// Test-only readback of the owner-thread provider image.  This is deliberately a diagnostic
/// projection: it carries no provider or admission authority and is not compiled for callers.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapacityLedgerTestSnapshot {
    claims: Vec<(
        AdmissionClaimId,
        VolumeIdentity,
        super::full_capacity::FullCapacityAllocationClass,
        u64,
    )>,
    dispositions: Vec<capacity_provider::ProvisionalLeaseDisposition>,
    next_token: u64,
}

#[allow(dead_code)]
enum JournalCommand {
    #[cfg(test)]
    InstallRecoveryAuditFixture {
        operation: OperationId,
        wire: Vec<u8>,
        corrupt_text_bytes: usize,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    #[cfg(test)]
    InstallHistoricalClaimFixture {
        operation: OperationId,
        sequence: i64,
        wire: Vec<u8>,
        claim: Uuid,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    #[cfg(test)]
    InspectRetirementWholeJournal {
        input: InputPermit,
        result: SyncSender<Result<([u64; 5], Option<i64>), OperationJournalOwnerError>>,
    },
    #[cfg(all(test, unix))]
    InspectCurrentNativeReserve {
        volume: VolumeIdentity,
        input: InputPermit,
        result: SyncSender<
            Result<current_source_native_fault_tests::NativeSnapshot, OperationJournalOwnerError>,
        >,
    },
    #[cfg(all(test, unix))]
    CurrentRefusedPrefix {
        request: SourceAuthorityRegistrationRequest,
        app: std::path::PathBuf,
        panic_after_retention: bool,
        probe: Option<Arc<Mutex<Option<current_source_custody_tests::Snapshot>>>>,
        input: InputPermit,
        result: SyncSender<Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError>>,
    },
    #[cfg(all(test, unix))]
    InspectCurrentRefusedPrefix {
        volume: VolumeIdentity,
        input: InputPermit,
        result:
            SyncSender<Result<current_source_custody_tests::Snapshot, OperationJournalOwnerError>>,
    },

    PrepareRecordingCustody {
        operation: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        input: InputPermit,
        result: SyncSender<Result<RecordingProducerCustody, OperationJournalOwnerError>>,
    },
    ValidateRecordingCustody {
        operation: OperationId,
        digest: [u8; 32],
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    PrepareLegacyCustody {
        source: crate::sample_sources::db::LegacySourceReadOwner,
        participant: super::ParticipantId,
        command: crate::sample_sources::db::SourceCommandId,
        input: InputPermit,
        result: SyncSender<Result<LegacyAdoptionCustody, OperationJournalOwnerError>>,
    },
    AdmitLegacyInitial {
        command: crate::sample_sources::db::SourceCommandId,
        digest: [u8; 32],
        deadline: std::time::Instant,
        input: InputPermit,
        result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    },
    PrepareLegacyVolumes {
        command: crate::sample_sources::db::SourceCommandId,
        digest: [u8; 32],
        input: InputPermit,
        result: SyncSender<Result<LegacyAdoptionCustody, OperationJournalOwnerError>>,
    },
    ValidateLegacyCustody {
        command: crate::sample_sources::db::SourceCommandId,
        digest: [u8; 32],
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    PrepareWalOwners {
        preparation: WalPreparationInput,
        input: super::wal_admission::JournalInput,
        result: SyncSender<Result<WalPreparationHandle, OperationJournalOwnerError>>,
    },
    RetainRecording {
        operation_id: OperationId,
        encoded: Vec<u8>,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        input: InputPermit,
        result: SyncSender<Result<RecordingChunkReceipt, OperationJournalOwnerError>>,
    },
    RecordingRestartAudit {
        operation_id: OperationId,
        input: InputPermit,
        result: SyncSender<
            Result<
                super::recording_commands::prefix::RecordingPrefixSeal,
                OperationJournalOwnerError,
            >,
        >,
    },
    RecordingRecovery {
        operation_id: OperationId,
        after_sequence: Option<i64>,
        input: InputPermit,
        result: SyncSender<Result<RecordingChunkPage, OperationJournalOwnerError>>,
    },
    AuthenticateSourceMetadata {
        candidate: crate::sample_sources::db::SourceRegistryMetadataCandidate,
        input: InputPermit,
        result: SyncSender<
            Result<
                crate::sample_sources::db::SourceRegistryMetadataPermit,
                OperationJournalOwnerError,
            >,
        >,
    },
    RegisterSource {
        request: SourceAuthorityRegistrationRequest,
        input: InputPermit,
        result: SyncSender<Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError>>,
    },
    BeginSourceDrain {
        receipt: SourceAuthorityRegistrationReceipt,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    FinalizeSourceRetirement {
        receipt: SourceAuthorityRegistrationReceipt,
        acknowledgement: SourceAuthorityRetirementAck,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    PersistIntent {
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<Result<JournalInsertOutcome, OperationJournalOwnerError>>,
    },
    PersistFull {
        expected_sequence: Option<i64>,
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<Result<FullProgressWriteOutcome, OperationJournalOwnerError>>,
    },
    Cancel {
        operation_id: OperationId,
        expected_sequence: i64,
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<Result<JournalCancelRequestReceipt, OperationJournalOwnerError>>,
    },
    Retry {
        operation_id: OperationId,
        expected_sequence: i64,
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<Result<JournalRetryReceipt, OperationJournalOwnerError>>,
    },
    ChunkExhaustion {
        operation_id: OperationId,
        expected_sequence: i64,
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<
            Result<(JournalRetryReceipt, OperationDisposition), OperationJournalOwnerError>,
        >,
    },
    AdmitFull {
        args: UniversalCommandArgs,
        participants: ParticipantSet,
        input: InputPermit,
        result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    },
    AdmitPrepared {
        prepared: PreparedAdmissionInput,
        input: InputPermit,
        result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    },
    ClaimChunk {
        request: ChunkClaimRequest,
        input: InputPermit,
        result: SyncSender<Result<FundedChunkClaimRecord, OperationJournalOwnerError>>,
    },
    IssueChunk {
        chunk_id: ChunkId,
        operation_id: OperationId,
        input: InputPermit,
        result: SyncSender<Result<ChunkWritePermit, OperationJournalOwnerError>>,
    },
    IssueLiveChunk {
        chunk_id: ChunkId,
        operation_id: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        spec: LiveChunkEffectSpec,
        input: InputPermit,
        result: SyncSender<Result<LiveChunkEffectGrant, OperationJournalOwnerError>>,
    },
    CheckpointStaging {
        target: PreparedPublicationTarget,
        charge_selector: publication_charge::Selector,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        input: InputPermit,
        result: SyncSender<Result<StagingPublicationGrant, OperationJournalOwnerError>>,
    },
    CheckpointLiveChunkStaging {
        chunk_id: ChunkId,
        issue_id: Uuid,
        validation: FileOperationResult,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        input: InputPermit,
        result: SyncSender<Result<ChunkStagingReceipt, OperationJournalOwnerError>>,
    },
    BeginChunkWrite {
        permit: ChunkWritePermit,
        identity: ChunkWriteIdentity,
        logical_bytes: u64,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    ReleaseChunk {
        permit: ChunkWritePermit,
        release_id: Uuid,
        input: InputPermit,
        result: SyncSender<Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError>>,
    },
    RetryChunkRelease {
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
        input: InputPermit,
        result: SyncSender<Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError>>,
    },
    CancelChunk {
        chunk_id: ChunkId,
        operation_id: OperationId,
        release_id: Uuid,
        input: InputPermit,
        result: SyncSender<Result<OrdinaryClaimReleaseReceipt, OperationJournalOwnerError>>,
    },
    #[cfg(test)]
    InjectActiveRecoveryCharge {
        operation_id: OperationId,
        volume: VolumeIdentity,
        budget_bytes: u64,
        input: InputPermit,
        result: SyncSender<Result<(), OperationJournalOwnerError>>,
    },
    #[cfg(test)]
    InspectCapacity {
        input: InputPermit,
        result: SyncSender<Result<CapacityLedgerTestSnapshot, OperationJournalOwnerError>>,
    },
    Get {
        operation_id: OperationId,
        input: InputPermit,
        result: SyncSender<Result<Option<StoredOperationRecord>, OperationJournalOwnerError>>,
    },
    Recovery {
        cursor: Option<RecoveryCursor>,
        input: InputPermit,
        result: SyncSender<Result<RecoveryPage, OperationJournalOwnerError>>,
    },
    ObserveCopy {
        operation_id: OperationId,
        harvest_key: super::work_plan::WorkUnitKey,
        observer_owner: std::sync::Weak<OwnerShared>,
        input: InputPermit,
        result: SyncSender<Result<Option<FrozenCopyObservation>, OperationJournalOwnerError>>,
    },
    GetFull {
        operation_id: OperationId,
        input: InputPermit,
        result: SyncSender<Result<Option<StoredFullProgressRecord>, OperationJournalOwnerError>>,
    },
    RecoveryStatus {
        cursor: Option<RecoveryCursor>,
        input: InputPermit,
        result: SyncSender<Result<super::RecoveryStatusPage, OperationJournalOwnerError>>,
    },
    CoordinatorRecovery {
        cursor: Option<RecoveryCursor>,
        input: InputPermit,
        result: SyncSender<Result<CoordinatorRecoveryPage, OperationJournalOwnerError>>,
    },
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerLifecycle {
    Initializing = 0,
    Available = 1,
    Unavailable = 2,
    Closing = 3,
    Closed = 4,
}

impl OwnerLifecycle {
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
    store: OperationJournalStore,
    command_rx: OwnerCommandReceivers,
    shared: Arc<OwnerShared>,
    #[cfg(any(test, feature = "test-support"))] command_gate: Option<(
        Arc<std::sync::Barrier>,
        Arc<std::sync::Barrier>,
    )>,
    #[cfg(any(test, feature = "test-support"))] panic_on_command: bool,
    #[cfg(any(test, feature = "test-support"))] bootstrap_capacity: bool,
    #[cfg(any(test, feature = "test-support"))] bootstrap_registry_path: Option<std::path::PathBuf>,
    #[cfg(test)] prepared_admission_id_allocations: Arc<AtomicUsize>,
    #[cfg(test)] synthetic_chunk_write_invocations: Arc<AtomicUsize>,
) {
    #[cfg(test)]
    let runtime_result = OwnerRuntime::new_with_id_counter(
        store,
        prepared_admission_id_allocations,
        synthetic_chunk_write_invocations,
    );
    #[cfg(not(test))]
    let runtime_result = OwnerRuntime::new(store);
    let mut runtime = match runtime_result {
        Ok(runtime) => runtime,
        Err(reason) => {
            shared.mark_unavailable(&format!("owner runtime bootstrap failed: {reason}"));
            return;
        }
    };

    if let Err(error) = runtime.attach_resource_custody(&shared) {
        shared.mark_unavailable(&format!("publication resource header unavailable: {error}"));
        return;
    }

    #[cfg(test)]
    if shared.startup_component.load(Ordering::Acquire) {
        let input = match shared.startup_input() {
            Ok(input) => input,
            Err(_) => {
                shared.mark_unavailable(startup_credit::REFUSAL);
                return;
            }
        };
        match runtime.rebuild_capacity_image_with_funding(Some(&input)) {
            Ok(()) => {
                shared.startup_component_claims.store(
                    runtime
                        .capacity_provider
                        .as_ref()
                        .unwrap()
                        .outstanding_claim_count_for_test(),
                    Ordering::Release,
                );
                shared.mark_startup_unavailable(
                    &"funded startup component verified; physical admission unsupported",
                    &input,
                );
            }
            Err(error) => shared.mark_startup_unavailable(&error, &input),
        }
        // Test-only actual worker component. It cannot activate the owner or issue IDs.
        return;
    }

    #[cfg(any(test, feature = "test-support"))]
    let bootstrap_capacity = bootstrap_capacity;
    #[cfg(any(test, feature = "test-support"))]
    let bootstrap_registry_path = bootstrap_registry_path;
    #[cfg(any(test, feature = "test-support"))]
    if bootstrap_capacity {
        let volume_owner = match runtime
            .store()
            .acquire_profile_journal_volume_owner(bootstrap_registry_path.as_deref())
        {
            Ok(volume_owner) => volume_owner,
            Err(error) => {
                shared.mark_unavailable(&format!(
                    "profile/journal volume ownership bootstrap failed: {error}"
                ));
                return;
            }
        };
        runtime.install_volume_owner(volume_owner);
        let reserve = match runtime.store().bootstrap_profile_journal_reserve(
            runtime.volume_owner(),
            bootstrap_registry_path.as_deref(),
        ) {
            Ok(reserve) => reserve,
            Err(error) => {
                shared.mark_unavailable(&format!(
                    "profile/journal reserve bootstrap failed: {error}"
                ));
                return;
            }
        };
        runtime.install_reserve(reserve);
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        let volume_owner = match runtime.store().acquire_profile_journal_volume_owner() {
            Ok(volume_owner) => volume_owner,
            Err(error) => {
                shared.mark_unavailable(&format!(
                    "profile/journal volume ownership bootstrap failed: {error}"
                ));
                return;
            }
        };
        runtime.install_volume_owner(volume_owner);
        let reserve = match runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner())
        {
            Ok(reserve) => reserve,
            Err(error) => {
                shared.mark_unavailable(&format!(
                    "profile/journal reserve bootstrap failed: {error}"
                ));
                return;
            }
        };
        runtime.install_reserve(reserve);
    }

    #[cfg(any(test, feature = "test-support"))]
    if bootstrap_capacity {
        let input = match shared.startup_input() {
            Ok(input) => input,
            Err(_) => {
                shared.mark_unavailable(startup_credit::REFUSAL);
                return;
            }
        };
        if let Err(error) = runtime.rebuild_capacity_with_funding(Some(&input)) {
            shared.mark_startup_unavailable(
                &format_args!("ordinary-claim capacity rebuild bootstrap failed: {error}"),
                &input,
            );
            return;
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        let input = match shared.startup_input() {
            Ok(input) => input,
            Err(_) => {
                shared.mark_unavailable(startup_credit::REFUSAL);
                return;
            }
        };
        if let Err(error) = runtime.rebuild_capacity_with_funding(Some(&input)) {
            shared.mark_startup_unavailable(
                &format_args!("ordinary-claim capacity rebuild bootstrap failed: {error}"),
                &input,
            );
            return;
        }
    }

    // Capture only the original retained guard, after every bootstrap check.
    // No DTO, current global profile lookup, Source row, or UI request can supply
    // this identity. The fixed OnceLock backing was prepaid on the original pool.
    let acquisition = runtime
        .final_profile_guard
        .as_ref()
        .and_then(|guard| Uuid::parse_str(guard.owner_identity()).ok());
    let Some(acquisition) = acquisition else {
        shared.mark_unavailable("original journal profile acquisition is invalid");
        return;
    };
    if shared
        .original_profile_acquisition
        .set(acquisition)
        .is_err()
    {
        shared.mark_unavailable("original journal profile acquisition already published");
        return;
    }

    let mut starting = Err(shared.lifecycle.load(Ordering::Acquire));
    shared.resource_admission.activate(|| {
        starting = shared.lifecycle.compare_exchange(
            OwnerLifecycle::Initializing as u8,
            OwnerLifecycle::Available as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        starting.is_ok()
    });
    if let Err(state) = starting {
        if OwnerLifecycle::from_u8(state) != OwnerLifecycle::Closing {
            command_rx.reject_pending(|| OperationJournalOwnerError::Closed);
            return;
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    let mut command_gate = command_gate;
    #[cfg(any(test, feature = "test-support"))]
    let panic_on_command = panic_on_command;
    let mut orderly_shutdown = true;
    loop {
        let Some(command) = command_rx.recv_with_idle(|| runtime.poll_original_work()) else {
            if let Some(collector) = &runtime.wal_preparation {
                collector.cancel();
            }
            // Pre-intent drain retains the original journal resources while original actors
            // confirm cleanup. No source/slot/native mutex is held by this polling loop.
            while runtime.wal_preparation.is_some() {
                runtime.poll_wal_preparation();
                if runtime.wal_preparation.is_some() {
                    thread::park_timeout(CONTROL_RECEIVE_POLL);
                }
            }
            break;
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Some((ready, release)) = command_gate.take() {
            ready.wait();
            release.wait();
        }
        #[cfg(any(test, feature = "test-support"))]
        if panic_on_command {
            panic!("operation journal owner panic fixture");
        }
        let panicked = execute_command(command, &mut runtime);
        if panicked {
            orderly_shutdown = false;
            shared.mark_unavailable("operation journal owner command panicked");
            command_rx.reject_pending(|| OperationJournalOwnerError::WorkerPanicked);
            break;
        }
    }
    runtime.poll_publication_completions();
    if !runtime.resource_admission.close_and_wait() {
        orderly_shutdown = false;
        if OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
            != OwnerLifecycle::Unavailable
        {
            shared.mark_unavailable("publication resource use is unknown or unsettled");
        }
    }
    if runtime.wal_preparation_unconfirmed {
        orderly_shutdown = false;
        shared.mark_unavailable("original WAL preparation cleanup remains unconfirmed");
    }
    if orderly_shutdown {
        if let Err(error) = runtime.handoff_volume_to_clean_owner() {
            tracing::warn!(error = %error, "clean volume ownership handoff was not completed");
        }
    }
    drop(runtime);
    if OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
        != OwnerLifecycle::Unavailable
    {
        shared
            .lifecycle
            .store(OwnerLifecycle::Closed as u8, Ordering::Release);
    }
}

// Move each payload into its executed closure: nonescaping backing drops before the debit.
// Returned outputs retain their independent result/producer custody. Input is not settlement.
fn execute_command(command: JournalCommand, runtime: &mut OwnerRuntime) -> bool {
    match command {
        #[cfg(test)]
        JournalCommand::InstallRecoveryAuditFixture {
            operation,
            wire,
            corrupt_text_bytes,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime
                    .store()
                    .replace_historical_row_fixture_for_test(operation, 1, &wire)?;
                runtime.store().install_recovery_audit_fixture_for_test(
                    operation,
                    1,
                    corrupt_text_bytes,
                )
            })
        }
        #[cfg(test)]
        JournalCommand::InstallHistoricalClaimFixture {
            operation,
            sequence,
            wire,
            claim,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime
                    .store()
                    .install_historical_claim_fixture(operation, sequence, &wire, claim)
            })
        }

        JournalCommand::PrepareRecordingCustody {
            operation,
            registration,
            source_pin,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.prepare_recording_custody(operation, registration, source_pin, input)
        }),
        JournalCommand::ValidateRecordingCustody {
            operation,
            digest,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.validate_recording_custody(operation, digest, input)
        }),
        JournalCommand::PrepareLegacyCustody {
            source,
            participant,
            command,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.prepare_legacy_custody(source, participant, command, input)
        }),
        JournalCommand::AdmitLegacyInitial {
            command,
            digest,
            deadline,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.admit_legacy_initial(command, digest, deadline, input)
        }),
        JournalCommand::PrepareLegacyVolumes {
            command,
            digest,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.prepare_legacy_volumes(command, digest, input)
        }),
        JournalCommand::ValidateLegacyCustody {
            command,
            digest,
            input,
            result,
        } => execute_store_result(result, move || {
            let _input = input;
            runtime.validate_legacy_custody(command, digest)
        }),
        JournalCommand::PrepareWalOwners {
            preparation,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.begin_wal_preparation(preparation, input)
        }),
        JournalCommand::AuthenticateSourceMetadata {
            candidate,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.authenticate_source_registry_metadata(candidate)
            })
        }
        #[cfg(all(test, unix))]
        JournalCommand::CurrentRefusedPrefix {
            request,
            app,
            panic_after_retention,
            probe,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.current_refused_prefix_for_test(
                current_source_metadata::CurrentInput::new(input),
                request,
                &app,
                panic_after_retention,
                probe,
            )
        }),
        #[cfg(all(test, unix))]
        JournalCommand::InspectCurrentNativeReserve {
            volume,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.inspect_current_native_reserve(&volume)
            })
        }
        #[cfg(all(test, unix))]
        JournalCommand::InspectCurrentRefusedPrefix {
            volume,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.inspect_current_refused_prefix(&volume)
            })
        }
        JournalCommand::RegisterSource {
            request,
            input,
            result,
        } => {
            let input = current_source_metadata::CurrentInput::new(input);
            execute_store_result(result, move || {
                runtime.register_source_authority_funded(Some(input), request)
            })
        }
        JournalCommand::BeginSourceDrain {
            receipt,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || runtime.begin_source_drain(receipt))
        }
        JournalCommand::FinalizeSourceRetirement {
            receipt,
            acknowledgement,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.finalize_source_retirement(receipt, acknowledgement)
            })
        }
        JournalCommand::RetainRecording {
            operation_id,
            encoded,
            registration,
            source_pin,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let operation = runtime.store().get(operation_id)?.ok_or_else(|| {
                    OperationJournalOwnerError::ChunkClaim {
                        reason: "recording operation absent".into(),
                    }
                })?;
                let Some(UniversalCommandArgs::Recording(proposal)) =
                    operation.universal_command_args()
                else {
                    return Err(OperationJournalOwnerError::ChunkClaim {
                        reason: "recording proposal absent".into(),
                    });
                };
                let chunk = RecordingCapturedChunk::decode(&encoded, proposal).map_err(|e| {
                    OperationJournalOwnerError::ChunkClaim {
                        reason: e.to_string(),
                    }
                })?;
                runtime.retain_recording_chunk(&chunk, registration, source_pin)
            })
        }
        JournalCommand::RecordingRestartAudit {
            operation_id,
            input,
            result,
        } => {
            runtime.start_recording_restart_audit(operation_id, input, result);
            false
        }
        JournalCommand::RecordingRecovery {
            operation_id,
            after_sequence,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime
                    .store()
                    .recording_chunk_page(operation_id, after_sequence)
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::PersistIntent {
            encoded,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let record = decode(&encoded)?;
                runtime
                    .publication_fences
                    .close_for_mutation(record.operation_id)?;
                Ok::<JournalInsertOutcome, OperationJournalOwnerError>(
                    runtime.store().insert_intent_with_outcome(&record),
                )
            })
        }
        JournalCommand::PersistFull {
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let record = decode_full_progress(&encoded).map_err(|error| {
                    OperationJournalOwnerError::InvalidInput {
                        reason: error.to_string(),
                    }
                })?;
                runtime
                    .publication_fences
                    .close_for_mutation(record.snapshot().intent().operation_id())?;
                runtime
                    .store()
                    .persist_full(&record, expected_sequence)
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::Cancel {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let observation = decode_control_input(&encoded)?;
                runtime.request_cancel_before_publish(operation_id, expected_sequence, observation)
            })
        }
        JournalCommand::Retry {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let observation = decode_control_input(&encoded)?;
                runtime
                    .publication_fences
                    .close_for_mutation(operation_id)?;
                runtime
                    .store()
                    .record_retry_pending(operation_id, expected_sequence, observation)
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::ChunkExhaustion {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                let observation = decode_control_input(&encoded)?;
                runtime
                    .publication_fences
                    .close_for_mutation(operation_id)?;
                runtime
                    .store()
                    .record_post_intent_chunk_exhaustion(
                        operation_id,
                        expected_sequence,
                        observation,
                    )
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::AdmitFull {
            args,
            participants,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                if matches!(&args, UniversalCommandArgs::SourceRegistryRetirement(_)) {
                    return Err(PreparedAdmissionError::Unsupported {
                        reason: "registry retirement requires the original all-owner settlement latch and qualified lifecycle admission".into(),
                    }.into());
                }
                if matches!(&args, UniversalCommandArgs::SourceLegacyAdoption(_)) {
                    return Err(PreparedAdmissionError::Unsupported {
                        reason: "legacy adoption requires the original-owner capacity and durable admission bridge".into(),
                    }.into());
                }
                if matches!(&args, UniversalCommandArgs::SourceDerivedLayout(_)) {
                    return Err(PreparedAdmissionError::Unsupported {
                        reason: "source layout requires the actual retained all-owner pre-intent admission latch".into(),
                    }.into());
                }
                runtime.store().validate_for_admission()?;
                derive_work_plan(&args, &participants).map_err(|error| {
                    OperationJournalOwnerError::InvalidInput {
                        reason: error.to_string(),
                    }
                })?;
                capacity_provider::production_admission_gate()
                    .map_err(|_| OperationJournalOwnerError::CapacityProviderUnavailable)?;
                Err(OperationJournalOwnerError::CapacityProviderUnavailable)
            })
        }
        JournalCommand::AdmitPrepared {
            prepared,
            input,
            result,
        } => {
            if matches!(
                prepared.args,
                UniversalCommandArgs::SourceRegistryRetirement(_)
            ) {
                let panic_result = result.clone();
                match panic::catch_unwind(AssertUnwindSafe(|| {
                    runtime.begin_retirement_admission(prepared, input, result)
                })) {
                    Ok(()) => false,
                    Err(_) => {
                        let _ = panic_result.send(Err(OperationJournalOwnerError::WorkerPanicked));
                        true
                    }
                }
            } else {
                // Keep terminal backing through send, including disposal of a SendError payload.
                let reply = input.historical_reply.clone();
                let panicked = execute_store_result(result, move || {
                    runtime.admit_prepared_with_funding(prepared, Some(&input))
                });
                drop(reply);
                panicked
            }
        }
        JournalCommand::ClaimChunk {
            request,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.claim_chunk_with_funding(request, Some(&input))
        }),
        JournalCommand::IssueChunk {
            chunk_id,
            operation_id,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.issue_chunk_with_funding(chunk_id, operation_id, Some(&input))
        }),
        JournalCommand::IssueLiveChunk {
            chunk_id,
            operation_id,
            registration,
            source_pin,
            spec,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.issue_live_chunk_effect(
                    chunk_id,
                    operation_id,
                    registration,
                    source_pin,
                    spec,
                )
            })
        }
        JournalCommand::CheckpointStaging {
            target,
            charge_selector,
            registration,
            source_pin,
            input,
            result,
        } => execute_store_result(result, move || {
            runtime.checkpoint_and_grant_staging(
                target,
                charge_selector,
                registration,
                source_pin,
                input,
            )
        }),
        JournalCommand::CheckpointLiveChunkStaging {
            chunk_id,
            issue_id,
            validation,
            registration,
            source_pin,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.checkpoint_live_chunk_staging(
                    chunk_id,
                    issue_id,
                    validation,
                    registration,
                    source_pin,
                )
            })
        }
        JournalCommand::BeginChunkWrite {
            permit,
            identity,
            logical_bytes,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.begin_chunk_write(permit, identity, logical_bytes)
            })
        }
        JournalCommand::ReleaseChunk {
            permit,
            release_id,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || runtime.release_chunk(permit, release_id))
        }
        JournalCommand::RetryChunkRelease {
            chunk_id,
            operation_id,
            release_id,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.retry_chunk_release(chunk_id, operation_id, release_id)
            })
        }
        JournalCommand::CancelChunk {
            chunk_id,
            operation_id,
            release_id,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.cancel_chunk(chunk_id, operation_id, release_id)
            })
        }
        #[cfg(test)]
        JournalCommand::InjectActiveRecoveryCharge {
            operation_id,
            volume,
            budget_bytes,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.store().insert_active_recovery_charge_for_test(
                    operation_id,
                    &volume,
                    budget_bytes,
                )
            })
        }
        #[cfg(test)]
        JournalCommand::InspectCapacity { input, result } => {
            let _input = input;
            execute_store_result(result, move || {
                Ok::<CapacityLedgerTestSnapshot, OperationJournalOwnerError>(
                    runtime.capacity_ledger_snapshot_for_test(),
                )
            })
        }
        #[cfg(test)]
        JournalCommand::InspectRetirementWholeJournal { input, result } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.store().retirement_whole_image_for_test()
            })
        }
        JournalCommand::Get {
            operation_id,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || runtime.store().get(operation_id))
        }
        JournalCommand::Recovery {
            cursor,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || runtime.store().scan_recovery_page(cursor))
        }
        JournalCommand::ObserveCopy {
            operation_id,
            harvest_key,
            observer_owner,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                frozen_copy::execute(runtime, operation_id, harvest_key, observer_owner)
            })
        }
        JournalCommand::GetFull {
            operation_id,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime
                    .store()
                    .get_full(operation_id)
                    .map(|row| row.map(|row| row.into_record()))
            })
        }
        JournalCommand::RecoveryStatus {
            cursor,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.store().scan_recovery_status_page(cursor)
            })
        }
        JournalCommand::CoordinatorRecovery {
            cursor,
            input,
            result,
        } => {
            let _input = input;
            execute_store_result(result, move || {
                runtime.store().scan_coordinator_recovery_page(cursor)
            })
        }
    }
}

fn decode_control_input<T>(encoded: &[u8]) -> Result<T, OperationJournalOwnerError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(encoded).map_err(|error| OperationJournalOwnerError::InvalidInput {
        reason: error.to_string(),
    })
}

fn execute_store_result<T, F, E>(
    result: SyncSender<Result<T, OperationJournalOwnerError>>,
    operation: F,
) -> bool
where
    F: FnOnce() -> Result<T, E>,
    E: Into<OperationJournalOwnerError>,
{
    match panic::catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => {
            let _ = result.send(value.map_err(Into::into));
            false
        }
        Err(_) => {
            let _ = result.send(Err(OperationJournalOwnerError::WorkerPanicked));
            true
        }
    }
}

fn reject_command(command: JournalCommand, error: OperationJournalOwnerError) {
    match &command {
        #[cfg(test)]
        JournalCommand::InstallRecoveryAuditFixture { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(test)]
        JournalCommand::InstallHistoricalClaimFixture { result, .. } => {
            let _ = result.send(Err(error));
        }

        JournalCommand::PrepareRecordingCustody { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ValidateRecordingCustody { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::PrepareLegacyCustody { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::AdmitLegacyInitial { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::PrepareLegacyVolumes { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ValidateLegacyCustody { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::PrepareWalOwners { result, .. } => {
            let _ = result.send(Err(error));
        }

        JournalCommand::AuthenticateSourceMetadata { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(all(test, unix))]
        JournalCommand::CurrentRefusedPrefix { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(all(test, unix))]
        JournalCommand::InspectCurrentNativeReserve { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(all(test, unix))]
        JournalCommand::InspectCurrentRefusedPrefix { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RegisterSource { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::BeginSourceDrain { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::FinalizeSourceRetirement { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RetainRecording { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RecordingRestartAudit { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RecordingRecovery { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::PersistIntent { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::PersistFull { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::Cancel { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::Retry { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ChunkExhaustion { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::AdmitFull { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::AdmitPrepared { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ClaimChunk { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::IssueChunk { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::IssueLiveChunk { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::CheckpointStaging { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::CheckpointLiveChunkStaging { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::BeginChunkWrite { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ReleaseChunk { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RetryChunkRelease { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::CancelChunk { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(test)]
        JournalCommand::InjectActiveRecoveryCharge { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(test)]
        JournalCommand::InspectCapacity { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(test)]
        JournalCommand::InspectRetirementWholeJournal { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::Get { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::Recovery { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::ObserveCopy { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::GetFull { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::RecoveryStatus { result, .. } => {
            let _ = result.send(Err(error));
        }
        JournalCommand::CoordinatorRecovery { result, .. } => {
            let _ = result.send(Err(error));
        }
    }
}

#[cfg(test)]
mod tests {
    mod native_retirement_inventory;
    #[cfg(target_os = "linux")]
    mod wal_preparation;

    mod command_drain_tests;
    mod control_input_tests;
    mod control_responsiveness_tests;
    mod native_read_capsule_tests;
    mod publication_charge_tests;
    mod publication_chunk_release_tests;
    mod publication_fence_tests;
    mod publication_input_tests;
    mod publication_mutation_tests;
    mod publication_uncertainty_tests;
    mod service_acquisition_tests;
    mod queue_metrics_integration {
        include!("owner/tests/queue_metrics_integration.rs");
    }
    mod capacity_startup_tests;
    mod control_backing_tests;
    mod historical_claim_admission_tests;
    mod namespace_backing_tests;
    mod provider_reconciliation_tests;
    mod resource_custody_tests;
    mod source_registry_handoff_tests;
    #[test]
    fn recording_dispatch_binding_checks_actual_bytes_range_output_and_root() {
        let (proposal, target) =
            super::super::recording_commands::recording_proposal_with_root_for_test();
        let captured = RecordingCapturedChunk::new(
            &proposal,
            uuid::Uuid::new_v4(),
            super::super::recording_commands::RecordingChunkRole::Header { revision: 0 },
            0,
            vec![3; 68],
        )
        .unwrap();
        // This pure binding fixture cannot issue a live grant or qualify physical storage.
        let mut spec = LiveChunkEffectSpec {
            effect_id: EffectId::new(),
            output: OutputIdentity::from_uuid(captured.output_id()),
            target,
            source_fences: vec![],
            offset: 0,
            logical_bytes: 68,
            payload_bytes: 68,
            payload_hash: *blake3::hash(captured.bytes()).as_bytes(),
        };
        assert!(recording_effect_matches(&proposal, &captured, &spec));
        spec.payload_hash[0] ^= 1;
        assert!(!recording_effect_matches(&proposal, &captured, &spec));
        spec.payload_hash = *blake3::hash(captured.bytes()).as_bytes();
        spec.offset = 1;
        assert!(!recording_effect_matches(&proposal, &captured, &spec));
        spec.offset = 0;
        spec.logical_bytes = 67;
        assert!(!recording_effect_matches(&proposal, &captured, &spec));
        spec.logical_bytes = 68;
        spec.output = OutputIdentity::new();
        assert!(!recording_effect_matches(&proposal, &captured, &spec));
        spec.output = OutputIdentity::from_uuid(captured.output_id());
        spec.target = RootRegistrationRef::synthetic_for_test();
        assert!(!recording_effect_matches(&proposal, &captured, &spec));
    }

    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    #[cfg(target_os = "macos")]
    use crate::durable_capacity::descriptor_capacity_facts;
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
    };
    #[cfg(target_os = "macos")]
    use crate::operation_journal::capacity_provider::{CapacityProvider, CapacityRebuildReadiness};
    use crate::operation_journal::full_progress_codec::StoredFullProgressRecord;
    #[cfg(target_os = "macos")]
    use crate::operation_journal::store::{OrdinaryClaimRebuildSnapshot, RebuildClaim};
    use crate::operation_journal::{
        AdmissionClaim, CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        EvidenceBatchId, ExternalCaptureBoundary, ExternalNativeString, ExternalRawEventKind,
        ExternalRawNativeAccounting, ExternalRawObservation, ExternalRawObservationEnvelope,
        ExternalRawObservationLimits, ExternalRawObservationMetadata,
        ExternalRawObservationProvenance, ExternalRawObservedPath, ExternalRawPathRole,
        ExternalReconciliationIntentArgs, ExternalRootBindingId, ExtractAndTrimIntentArgs,
        FiniteF32Bits, HistoryAction, HistoryDirection, HistoryRecoveryBackupObservation,
        HistoryRecoveryLeafObservation, HistoryRestoreTargetExpectation, HistoryScopeId,
        HistoryUndoRedoIntentArgs, HistoryWaveformRestore, ImportFileExpectation, ImportIntentArgs,
        MAX_DIAGNOSTIC_CODE_BYTES, NamespaceCommandArgs, NamespaceDirectoryEndpoint,
        NamespaceDirectoryExpectation, NamespaceDirectoryLocator, NamespaceEntryEndpoint,
        NamespaceEntryExpectation, NamespaceFileExpectation, NamespaceLeafName,
        NamespaceRenameIntentArgs, NamespaceTarget, OperationActor, OperationDisposition,
        OperationIntent, OperationKind, ParticipantCheckpoint, ParticipantCheckpointState,
        ParticipantDescriptor, ParticipantKind, ParticipantRequirement, ParticipantSet,
        RawWavExtractionRecipe, RecoveryCursor, RecoveryRowOutcome, RemovalCommandArgs,
        RetryErrorClass, RetryLeaseId, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9,
        SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15,
        SelectionExtractIntentArgs, SelectionNormalizeIntentArgs, SelectionOutputCommandArgs,
        SelectionOutputRecipe, StoredRelativePath, StoredSourceFence, TrashIntentArgs,
        UniversalCommandArgs, UniversalCommandKind, UniversalIntent,
        WaveformDuplicateDoubleIntentArgs, WaveformEditFrameRange, WaveformEditIntentArgs,
        WaveformEditOperationArgs, WaveformEditSettingsArgs, WaveformFileRevisionObservation,
        WaveformOutputSpec, WaveformRenderOutputSpec, WaveformSampleFormat, WaveformSourceSpec,
        decode,
    };
    #[cfg(target_os = "macos")]
    use crate::sample_sources::db::SourceDatabase;
    use crate::sample_sources::{SourceId, SourcePlaybackType};
    use crate::test_runtime::TestRuntimeGuard;
    use std::fs;
    use std::thread;

    struct Fixture {
        owner: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _config: ConfigBaseGuard,
        _base: tempfile::TempDir,
        _runtime: TestRuntimeGuard,
    }

    #[test]
    fn publication_namespace_exhaustion_after_commit_quarantines_original_runtime() {
        let fixture = fixture("publication-namespace-uncertain-original-runtime");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let runtime = OwnerRuntime::new(store).unwrap();
        assert!(runtime.resource_admission.activate(|| true));
        let result: Result<(), OperationJournalStoreError> =
            Err(OperationJournalStoreError::StagingNamespaceOutcomeUncertain);
        assert!(matches!(
            runtime.publication_write_result(result),
            Err(OperationJournalOwnerError::Store(
                OperationJournalStoreError::StagingNamespaceOutcomeUncertain
            ))
        ));
        assert!(matches!(
            runtime.resource_admission.validate_open(),
            Err(resource_custody::UseError::Unknown)
        ));
        runtime.publication_write_result(Ok(())).unwrap();
        assert!(!runtime.resource_admission.activate(|| true));
        assert!(!runtime.resource_admission.close_and_wait());
    }

    fn fixture(name: &str) -> Fixture {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().expect("temporary config base");
        let base_path = fs::canonicalize(base.path()).expect("canonical temporary config base");
        let config = ConfigBaseGuard::set(base_path);
        let profile = PersistenceProfileGuard::named(name);
        let owner = WritableProfileGuard::acquire_current().expect("profile owner");
        Fixture {
            owner,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }

    fn open_owner(name: &str) -> (Fixture, OperationJournalOwner) {
        let fixture = fixture(name);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        (fixture, owner)
    }

    pub(super) fn wait_until_available(owner: &OperationJournalOwner) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match owner.status() {
                OperationJournalOwnerStatus::Available => return,
                OperationJournalOwnerStatus::Unavailable { reason } => {
                    panic!("journal owner unavailable: {reason}")
                }
                status if std::time::Instant::now() >= deadline => {
                    panic!("journal owner status: {status:?}")
                }
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    #[test]
    fn current_source_registration_busy_preserves_confirmation_and_refusal_releases_debit() {
        let (fixture, mut owner) = open_owner("current-source-input-custody");
        let source_root = tempfile::tempdir().expect("source root");
        let database = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "current-input-source",
        )
        .expect("source database");
        let authority = database
            .prepare_source_authority()
            .expect("original authority");
        let request = authority.journal_request().expect("original confirmation");
        assert!(request.current_metadata_bytes().expect("bounded inventory") > CONTROL_INPUT_BYTES);
        let original = Arc::clone(&request.request);
        let confirmation = request.confirmation.clone();
        let client = owner.client();
        let ordinary_before = client.shared.input_bytes.lock().expect("budget").remaining[0];
        let occupied = client
            .shared
            .reserve_input(InputClass::Ordinary, ordinary_before)
            .expect("occupy original ordinary pool");
        assert!(matches!(client.register_source_authority(request),
            Err(OperationJournalOwnerQueueError::InputBytesFull { requested, .. })
                if requested > CONTROL_INPUT_BYTES));
        // Admission rejected before the one-shot proof was consumed, so the exact original
        // confirmation is still usable. Consuming it here performs no filesystem effect.
        drop(occupied);
        let proof = original
            .accept_confirmation(confirmation)
            .expect("Busy left proof unspent");
        drop(proof);
        drop(original);
        drop(authority);
        drop(database);
        assert!(owner.shutdown());
        drop(owner);
        drop(fixture);

        let (fixture, mut owner) = open_owner("current-source-refusal-debit");
        let source_root = tempfile::tempdir().expect("source root");
        let database = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "current-refusal-source",
        )
        .expect("source database");
        let authority = database
            .prepare_source_authority()
            .expect("original authority");
        let client = owner.client();
        let request = authority.journal_request().expect("original confirmation");
        let original = Arc::clone(&request.request);
        let confirmation = request.confirmation.clone();
        let ordinary_before = client.shared.input_bytes.lock().expect("budget").remaining[0];
        let response = client
            .register_source_authority(request)
            .expect("actual queue")
            .recv_timeout(Duration::from_secs(2))
            .expect("actual reply");
        assert!(matches!(
            response,
            Err(OperationJournalOwnerError::SourceAuthorityConflict { .. })
        ));
        // The result is sent before the registration closure is destroyed. A second real
        // worker command runs only after that closure has dropped. This test-only control
        // routing of a readonly point lookup leaves the measured ordinary credit unchanged.
        let mut barrier = client
            .submit_control(CONTROL_INPUT_BYTES, |input, result| JournalCommand::Get {
                operation_id: OperationId::new(),
                input,
                result,
            })
            .expect("actual worker barrier");
        assert!(
            barrier
                .recv_timeout(Duration::from_secs(2))
                .expect("barrier reply")
                .expect("readonly barrier")
                .is_none()
        );
        assert_eq!(
            client.shared.input_bytes.lock().expect("budget").remaining[0],
            ordinary_before
        );
        drop(
            original
                .accept_confirmation(confirmation)
                .expect("pre-effect refusal left proof unspent"),
        );
        drop(original);
        drop(authority);
        drop(database);
        assert!(owner.shutdown());
    }

    #[test]
    fn portable_journal_fixture_cannot_register_physical_source_authority() {
        let (fixture, mut owner) = open_owner("portable-registry-no-physical-authority");
        let source_root = tempfile::tempdir().expect("source root");
        let database = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "portable-registry-source",
        )
        .expect("profile-owned source database");
        let authority = database
            .prepare_source_authority()
            .expect("descriptor-bound source authority request");
        let request = authority.journal_request().expect("registration request");
        let error = owner
            .client()
            .register_source_authority(request)
            .expect("registration queues")
            .recv_timeout(Duration::from_secs(2))
            .expect("registration responds")
            .expect_err("non-capacity fixture cannot mint a physical registration receipt");
        assert!(matches!(
            error,
            OperationJournalOwnerError::SourceAuthorityConflict { reason }
                if reason == "profile volume ownership is unavailable"
        ));
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Available);
        drop(authority);
        drop(database);
        assert!(owner.shutdown());
    }

    fn wait_until_unavailable(owner: &OperationJournalOwner) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match owner.status() {
                OperationJournalOwnerStatus::Unavailable { reason } => return reason,
                OperationJournalOwnerStatus::Available => {
                    panic!("journal owner unexpectedly became available")
                }
                status if std::time::Instant::now() >= deadline => {
                    panic!("journal owner status: {status:?}")
                }
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    #[cfg(unix)]
    fn fixture_volume_registry(fixture: &Fixture) -> std::path::PathBuf {
        // macOS exposes the temporary root through /var -> /private/var. The
        // registry owner deliberately walks every path component without
        // following symlinks, so use the canonical retained fixture root.
        let base = fs::canonicalize(fixture._base.path()).expect("canonical fixture root");
        let path = base.join("volume-registry");
        fs::create_dir(&path).expect("fixture volume registry");
        assert!(
            fs::metadata(&path)
                .expect("fixture volume registry metadata")
                .is_dir(),
            "fixture volume registry must be a directory: {}",
            path.display()
        );
        path
    }

    #[cfg(target_os = "macos")]
    fn handoff_runtime(
        name: &str,
    ) -> (
        Fixture,
        OwnerRuntime,
        std::path::PathBuf,
        std::path::PathBuf,
        fs::File,
        VolumeIdentity,
    ) {
        let fixture = fixture(name);
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        let volume = volume_owner.volume().clone();
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);
        runtime.capacity_provider = Some(CapacityProvider::unavailable());
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        (
            fixture,
            runtime,
            registry_path,
            reserve_path,
            descriptor,
            volume,
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clean_handoff_refuses_dirty_boundaries_without_marker_or_lock_release() {
        enum Refusal {
            OrdinaryUnreleased,
            OrdinaryAuditPending,
            LiveProviderToken,
            ActiveSourceRegistration,
            IncompleteSourceBinding,
            ReplacedReserve,
        }

        let cases = [
            (
                "journal-owner-handoff-unreleased",
                Refusal::OrdinaryUnreleased,
            ),
            ("journal-owner-handoff-audit", Refusal::OrdinaryAuditPending),
            (
                "journal-owner-handoff-live-token",
                Refusal::LiveProviderToken,
            ),
            (
                "journal-owner-handoff-active-source",
                Refusal::ActiveSourceRegistration,
            ),
            (
                "journal-owner-handoff-incomplete-source",
                Refusal::IncompleteSourceBinding,
            ),
            (
                "journal-owner-handoff-replaced-reserve",
                Refusal::ReplacedReserve,
            ),
        ];
        for (name, refusal) in cases {
            let (fixture, mut runtime, registry_path, reserve_path, descriptor, volume) =
                handoff_runtime(name);
            match refusal {
                Refusal::OrdinaryUnreleased | Refusal::OrdinaryAuditPending => {
                    let operation_id = OperationId::new();
                    if matches!(refusal, Refusal::OrdinaryUnreleased) {
                        runtime
                            .store()
                            .insert_intent(&intent_record(operation_id))
                            .expect("ordinary proof operation");
                    }
                    runtime
                        .store()
                        .insert_ordinary_claim_for_test(
                            uuid::Uuid::new_v4(),
                            operation_id,
                            &volume,
                            "DestinationStaging",
                            4096,
                        )
                        .expect("ordinary refusal claim");
                }
                Refusal::LiveProviderToken => {
                    let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::
                        DestinationStaging;
                    let provider = CapacityProvider::unavailable();
                    provider
                        .install_rebuilt_claims(&OrdinaryClaimRebuildSnapshot::for_test(
                            vec![RebuildClaim::for_test(
                                OperationId::new(),
                                crate::operation_journal::AdmissionClaimId::new(),
                                volume.clone(),
                                class,
                                4096,
                                None,
                                false,
                            )],
                            CapacityRebuildReadiness::ready(),
                        ))
                        .expect("live provider token");
                    runtime.capacity_provider = Some(provider);
                }
                Refusal::ActiveSourceRegistration => {
                    let source_root = tempfile::tempdir().expect("source root");
                    let database = SourceDatabase::open_for_catalog_with_profile_guard(
                        source_root.path(),
                        &fixture.owner,
                        "handoff-active-source-catalog",
                    )
                    .expect("profile-owned source database");
                    let authority = database
                        .prepare_source_authority()
                        .expect("source authority");
                    let request = authority.journal_request().expect("journal request");
                    runtime
                        .register_source_authority(request)
                        .expect("active source registration");
                }
                Refusal::IncompleteSourceBinding => {
                    runtime.source_volumes.insert(
                        volume.clone(),
                        SourceVolumeBinding {
                            _current_input: None,
                            #[cfg(all(test, unix))]
                            _current_before_root_drop: None,
                            _current_root: None,
                            #[cfg(all(test, unix))]
                            _current_after_root_drop: None,
                            _current_volume_failure: Default::default(),
                            _current_reserve_failure: Default::default(),
                            owner: None,
                            reserve: None,
                            allocation_unit: Some(1),
                            readiness: SourceVolumeBindingReadiness::Incomplete,
                            registrations: BTreeSet::new(),
                        },
                    );
                }
                Refusal::ReplacedReserve => {
                    fs::remove_file(&reserve_path).expect("remove reserve entry");
                    fs::write(&reserve_path, b"replacement").expect("replace reserve entry");
                }
            }

            let error = runtime
                .handoff_volume_to_clean_owner()
                .expect_err("unsafe handoff must remain recovery-only");
            assert!(!runtime.volume_owner().marker_is_clean_for_test());
            assert!(!error.is_empty());
            let held = super::super::capacity_volume_owner::acquire_fixture_for_test(
                &descriptor,
                &registry_path,
                volume,
                fixture.owner.profile_root_identity(),
            )
            .expect_err("refused handoff retains volume lock");
            assert!(matches!(
                held,
                super::super::capacity_volume_owner::VolumeOwnershipError::OwnedByAnotherProcess { .. }
            ));
            drop(runtime);
            drop(fixture);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clean_handoff_with_exact_zero_proof_marks_then_reopens_new_generation() {
        let (fixture, mut runtime, registry_path, _reserve_path, descriptor, volume) =
            handoff_runtime("journal-owner-handoff-success");
        runtime
            .handoff_volume_to_clean_owner()
            .expect("empty durable state and fresh proof permit handoff");
        assert!(runtime.volume_owner().marker_is_clean_for_test());
        assert!(matches!(
            runtime.volume_owner().revalidate(),
            Err(super::super::capacity_volume_owner::VolumeOwnershipError::HandoffFenced { .. })
        ));
        drop(runtime);
        let restarted =
            super::super::capacity_volume_owner::acquire_fixture_with_clean_marker_for_test(
                &descriptor,
                &registry_path,
                volume,
                fixture.owner.profile_root_identity(),
                true,
            )
            .expect("clean marker restart");
        assert_eq!(restarted.owner_generation(), 2);
        drop(restarted);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_bootstraps_and_reopens_profile_journal_reserve() {
        let fixture = fixture("journal-owner-reserve-bootstrap");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            registry_path.clone(),
        )
        .expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();
        let (args, participants) = valid_full_admission_request();
        let mut admission = client
            .admit_full(args, participants)
            .expect("ordinary admission request");
        assert!(matches!(
            admission
                .recv_timeout(Duration::from_secs(2))
                .expect("ordinary admission result")
                .expect_err("ordinary admission remains unavailable"),
            OperationJournalOwnerError::CapacityProviderUnavailable
        ));
        assert!(owner.shutdown());

        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let first_metadata = fs::metadata(&reserve_path).expect("reserve after bootstrap");
        assert_eq!(
            first_metadata.len(),
            crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
        );
        let reserve_descriptor = fs::File::open(&reserve_path).expect("open reserve descriptor");
        let volume = descriptor_capacity_facts(&reserve_descriptor)
            .expect("reserve capacity facts")
            .identity;

        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let (binding_before, counts_before) = reopened_store
            .test_reserve_bootstrap_snapshot(&volume)
            .expect("fresh bootstrap snapshot");
        assert!(
            binding_before.is_some(),
            "fresh bootstrap must persist binding"
        );
        assert_eq!(counts_before, (0, 1, 0, 0));
        let mut reopened_owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            reopened_store,
            registry_path,
        )
        .expect("reopen journal owner");
        wait_until_available(&reopened_owner);
        assert!(reopened_owner.shutdown());

        let second_metadata = fs::metadata(&reserve_path).expect("reserve after reopen");
        assert_eq!(second_metadata.len(), first_metadata.len());
        let final_store = OperationJournalStore::open(&fixture.owner).expect("final store");
        let (binding_after, counts_after) = final_store
            .test_reserve_bootstrap_snapshot(&volume)
            .expect("reopened bootstrap snapshot");
        let binding_before = binding_before.expect("binding before restart");
        let binding_after = binding_after.expect("binding after restart");
        assert_eq!(binding_after.0, binding_before.0);
        assert_eq!(binding_after.1, binding_before.1);
        assert_eq!(binding_after.2, binding_before.2);
        assert_eq!(
            binding_after.3,
            binding_before
                .3
                .checked_add(1)
                .expect("generation increment")
        );
        assert_eq!(counts_after, counts_before);
        drop(final_store);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_refreshes_live_descriptor_facts_without_qualifying_admission() {
        let fixture = fixture("journal-owner-live-descriptor-refresh");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);

        let observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("retained descriptor refresh");
        assert_eq!(observation.facts.len(), 1);
        let (volume, facts) = observation.facts.iter().next().expect("profile facts");
        assert_eq!(volume, &runtime.volume_owner().volume().clone());
        assert_eq!(facts.identity, *volume);
        assert!(facts.free_bytes > 0);
        assert!(facts.allocation_unit > 0);
        let provenance = observation
            .provenance
            .get(volume)
            .expect("descriptor provenance");
        assert_eq!(
            provenance.profile_identity,
            fixture.owner.profile_root_identity()
        );
        assert_eq!(
            provenance.owner_generation,
            runtime.volume_owner().owner_generation()
        );
        assert!(!provenance.reserve_root_identity.is_empty());
        assert!(!provenance.reserve_file_identity.is_empty());
        assert_eq!(provenance.allocation_unit, facts.allocation_unit);
        assert!(!matches!(
            runtime.capacity_rebuild_readiness(),
            CapacityRebuildReadiness::Ready
        ));
        assert!(capacity_provider::production_admission_gate().is_err());

        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    fn test_witness_with_free_bytes(
        runtime: &mut OwnerRuntime,
        free_bytes: u64,
    ) -> Result<LiveAdmissionWitness<'_>, String> {
        let reader = move |reserve: &mut OwnedReserveAllocation,
                           profile_guard: &WritableProfileGuard|
              -> Result<VolumeFacts, String> {
            let facts = reserve
                .view(profile_guard)
                .fresh_capacity_facts()
                .map_err(|error| error.to_string())?;
            Ok(VolumeFacts {
                free_bytes,
                ..facts
            })
        };
        let observation = runtime.refresh_live_descriptor_capacity_inner(Some(&reader), true)?;
        runtime.live_admission_witness(observation)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chunk_issuance_witness_checks_retained_headroom_without_ledger_mutation() {
        let fixture = fixture("journal-owner-chunk-issuance-headroom");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);
        let observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("initial live descriptor refresh");
        let (volume, base_facts) = observation
            .facts
            .iter()
            .next()
            .map(|(volume, facts)| (volume.clone(), facts.clone()))
            .expect("profile facts");
        runtime.capacity_provider = Some(CapacityProvider::unavailable());
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            volume.clone(),
            base_facts.allocation_unit,
        );
        let operation_id = runtime
            .admit_prepared(input)
            .expect("initial operation admission");
        let operation = runtime
            .store()
            .get(operation_id)
            .expect("read admitted operation")
            .expect("admitted operation");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let request = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                base_facts.allocation_unit,
            )
            .expect("chunk claim"),
            participant_id,
            String::from("headroom-check"),
            0,
            base_facts.allocation_unit,
            base_facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("chunk request");
        runtime
            .claim_chunk(request.clone())
            .expect("post-intent chunk claim");
        let record = runtime
            .store()
            .read_chunk_claim(request.chunk_id())
            .expect("read chunk claim")
            .expect("chunk claim row");
        let held = runtime
            .capacity_provider
            .as_ref()
            .expect("provider")
            .outstanding(
            &volume,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
        );
        let claims_before = runtime
            .capacity_provider
            .as_ref()
            .expect("provider")
            .claim_count();

        let provider = runtime.capacity_provider.take().expect("provider");
        let enough = test_witness_with_free_bytes(
            &mut runtime,
            PROTECTED_FREE_SPACE_FLOOR
                .checked_add(held)
                .and_then(|bytes| bytes.checked_add(base_facts.allocation_unit))
                .expect("headroom range"),
        )
        .expect("full-floor witness with retained headroom");
        provider
            .validate_durable_chunk_issuance(&record, &enough)
            .expect("held charge fits above the floor");
        drop(enough);
        runtime.capacity_provider = Some(provider);
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider")
                .claim_count(),
            claims_before
        );
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider")
                .outstanding(&volume, record.allocation_class()),
            held
        );

        let provider = runtime.capacity_provider.take().expect("provider");
        let disk_pressure = test_witness_with_free_bytes(
            &mut runtime,
            PROTECTED_FREE_SPACE_FLOOR
                .checked_add(held.saturating_sub(1))
                .expect("pressure range"),
        )
        .expect("full-floor pressure witness");
        assert!(matches!(
            provider.validate_durable_chunk_issuance(&record, &disk_pressure),
            Err(CapacityProviderError::DiskPressureRecoveryOnly { .. })
        ));
        drop(disk_pressure);
        runtime.capacity_provider = Some(provider);

        let provider = runtime.capacity_provider.take().expect("provider");
        let low = test_witness_with_free_bytes(&mut runtime, PROTECTED_FREE_SPACE_FLOOR)
            .expect("full-floor low witness");
        assert!(matches!(
            provider.validate_durable_chunk_issuance(&record, &low),
            Err(CapacityProviderError::RecoveryReserveLow { .. })
        ));
        drop(low);
        runtime.capacity_provider = Some(provider);

        let wrong_unit_reader = |reserve: &mut OwnedReserveAllocation,
                                 profile_guard: &WritableProfileGuard|
         -> Result<VolumeFacts, String> {
            let facts = reserve
                .view(profile_guard)
                .fresh_capacity_facts()
                .map_err(|error| error.to_string())?;
            Ok(VolumeFacts {
                allocation_unit: facts
                    .allocation_unit
                    .checked_add(1)
                    .expect("wrong allocation unit"),
                ..facts
            })
        };
        assert!(
            runtime
                .refresh_live_descriptor_capacity_inner(Some(&wrong_unit_reader), true)
                .is_err()
        );

        let mut missing_observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("fresh observation for missing-facts fixture");
        missing_observation.facts.clear();
        let provider = runtime.capacity_provider.take().expect("provider");
        let missing = runtime
            .live_admission_witness(missing_observation)
            .expect("sealed witness retains authority context");
        assert!(matches!(
            provider.validate_durable_chunk_issuance(&record, &missing),
            Err(CapacityProviderError::FactsUnavailable { .. })
        ));
        drop(missing);
        runtime.capacity_provider = Some(provider);
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider")
                .claim_count(),
            claims_before
        );
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider")
                .outstanding(&volume, record.allocation_class()),
            held
        );

        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_refresh_faults_fail_closed_without_changing_ledger_or_readiness() {
        #[derive(Clone, Copy)]
        enum Fault {
            Unsupported,
            Offline,
            WrongIdentity,
            AllocationUnitChanged,
        }

        let fixture = fixture("journal-owner-live-descriptor-faults");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);

        let stale_observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("initial live descriptor refresh");
        let volume = runtime.volume_owner().volume().clone();
        let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging;
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&OrdinaryClaimRebuildSnapshot::for_test(
                vec![RebuildClaim::for_test(
                    OperationId::new(),
                    crate::operation_journal::AdmissionClaimId::new(),
                    volume.clone(),
                    class,
                    4096,
                    None,
                    false,
                )],
                CapacityRebuildReadiness::ready(),
            ))
            .expect("seed provider ledger");
        runtime.capacity_provider = Some(provider);
        let readiness =
            CapacityRebuildReadiness::recovery_only(vec![String::from("retained before fault")]);
        runtime.capacity_rebuild_readiness = readiness.clone();
        let outstanding_before = runtime
            .capacity_provider
            .as_ref()
            .expect("provider")
            .outstanding(&volume, class);

        let faults = [
            (Fault::Unsupported, "unsupported descriptor backend"),
            (
                Fault::Offline,
                "descriptor is unavailable because the volume is offline",
            ),
            (Fault::WrongIdentity, "fresh descriptor facts differ"),
            (
                Fault::AllocationUnitChanged,
                "fresh descriptor allocation unit differs",
            ),
        ];
        for (fault, expected_error) in faults {
            let reader = move |reserve: &mut OwnedReserveAllocation,
                               profile_guard: &WritableProfileGuard|
                  -> Result<VolumeFacts, String> {
                match fault {
                    Fault::Unsupported => Err(String::from("unsupported descriptor backend")),
                    Fault::Offline => Err(String::from(
                        "descriptor is unavailable because the volume is offline",
                    )),
                    Fault::WrongIdentity | Fault::AllocationUnitChanged => {
                        let facts = reserve
                            .view(profile_guard)
                            .fresh_capacity_facts()
                            .map_err(|error| error.to_string())?;
                        match fault {
                            Fault::WrongIdentity => Ok(VolumeFacts {
                                identity: VolumeIdentity {
                                    device: facts
                                        .identity
                                        .device
                                        .checked_add(1)
                                        .expect("fixture device identity increment"),
                                },
                                ..facts
                            }),
                            Fault::AllocationUnitChanged => Ok(VolumeFacts {
                                allocation_unit: facts
                                    .allocation_unit
                                    .checked_add(1)
                                    .expect("fixture allocation-unit increment"),
                                ..facts
                            }),
                            Fault::Unsupported | Fault::Offline => unreachable!(),
                        }
                    }
                }
            };

            let error = runtime
                .refresh_live_descriptor_capacity_with_reader(&reader)
                .expect_err("faulted descriptor refresh must fail closed");
            assert!(
                error.contains(expected_error),
                "unexpected fault diagnostic: {error}"
            );
            assert_eq!(
                runtime
                    .capacity_provider
                    .as_ref()
                    .expect("provider retained after failed refresh")
                    .outstanding(&volume, class),
                outstanding_before
            );
            assert_eq!(runtime.capacity_rebuild_readiness(), &readiness);
            assert_eq!(stale_observation.facts.len(), 1);
            assert!(capacity_provider::production_admission_gate().is_err());
        }

        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_refresh_rejects_replaced_descriptor_without_changing_live_state() {
        let fixture = fixture("journal-owner-live-descriptor-replacement");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);

        runtime
            .refresh_live_descriptor_capacity()
            .expect("initial live descriptor refresh");

        let volume = runtime.volume_owner().volume().clone();
        let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging;
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&OrdinaryClaimRebuildSnapshot::for_test(
                vec![RebuildClaim::for_test(
                    OperationId::new(),
                    crate::operation_journal::AdmissionClaimId::new(),
                    volume.clone(),
                    class,
                    4096,
                    None,
                    false,
                )],
                CapacityRebuildReadiness::ready(),
            ))
            .expect("seed provider ledger");
        runtime.capacity_provider = Some(provider);
        runtime.capacity_rebuild_readiness =
            CapacityRebuildReadiness::recovery_only(vec![String::from("retained before refresh")]);
        let before = runtime
            .capacity_provider
            .as_ref()
            .expect("provider")
            .outstanding(&volume, class);

        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        fs::remove_file(&reserve_path).expect("remove retained namespace entry");
        fs::write(&reserve_path, b"replacement").expect("install replacement namespace entry");

        let error = runtime
            .refresh_live_descriptor_capacity()
            .expect_err("replacement must fail closed");
        assert!(error.contains("retained reserve facts"));
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider retained after failed refresh")
                .outstanding(&volume, class),
            before
        );
        assert_eq!(
            runtime.capacity_rebuild_readiness(),
            &CapacityRebuildReadiness::recovery_only(vec![
                String::from("retained before refresh",)
            ])
        );

        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_refresh_stages_all_retained_volumes_before_publishing_observation() {
        let fixture = fixture("journal-owner-live-descriptor-atomic-refresh");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);

        let volume = runtime.volume_owner().volume().clone();
        let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging;
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&OrdinaryClaimRebuildSnapshot::for_test(
                vec![RebuildClaim::for_test(
                    OperationId::new(),
                    crate::operation_journal::AdmissionClaimId::new(),
                    volume.clone(),
                    class,
                    4096,
                    None,
                    false,
                )],
                CapacityRebuildReadiness::ready(),
            ))
            .expect("seed provider ledger");
        runtime.capacity_provider = Some(provider);
        runtime.capacity_rebuild_readiness = CapacityRebuildReadiness::ready();

        let invalid_volume = VolumeIdentity {
            device: volume
                .device
                .checked_add(1)
                .expect("fixture volume increment"),
        };
        runtime.source_volumes.insert(
            invalid_volume,
            SourceVolumeBinding {
                _current_input: None,
                #[cfg(all(test, unix))]
                _current_before_root_drop: None,
                _current_root: None,
                #[cfg(all(test, unix))]
                _current_after_root_drop: None,
                _current_volume_failure: Default::default(),
                _current_reserve_failure: Default::default(),
                owner: None,
                reserve: None,
                allocation_unit: Some(4096),
                readiness: SourceVolumeBindingReadiness::Incomplete,
                registrations: BTreeSet::new(),
            },
        );
        let error = runtime
            .refresh_live_descriptor_capacity()
            .expect_err("incomplete retained volume must reject the complete observation");
        assert!(error.contains("incomplete retained binding"));
        assert_eq!(
            runtime
                .capacity_provider
                .as_ref()
                .expect("provider retained after atomic failure")
                .outstanding(&volume, class),
            4096
        );
        assert_eq!(
            runtime.capacity_rebuild_readiness(),
            &CapacityRebuildReadiness::Ready
        );

        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_replays_source_registration_and_retires_exactly_once() {
        let fixture = fixture("journal-owner-source-authority");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            registry_path.clone(),
        )
        .expect("journal owner");
        wait_until_available(&owner);

        let source_root = tempfile::tempdir().expect("source root");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "source-authority-catalog",
        )
        .expect("profile-owned source database");
        let mut authority = database
            .prepare_source_authority()
            .expect("source authority");
        let initial_request = authority.journal_request().expect("journal request");
        let expired_request = SourceAuthorityRegistrationRequest {
            request: Arc::clone(&initial_request.request),
            confirmation: initial_request
                .confirmation
                .clone()
                .with_expired_deadline_for_tests(),
        };
        let client = owner.client();
        let mut expired = client
            .register_source_authority(expired_request)
            .expect("expired registration request");
        assert!(matches!(
            expired
                .recv_timeout(Duration::from_secs(2))
                .expect("expired registration result"),
            Err(OperationJournalOwnerError::SourceAuthority(
                SourceAuthorityError::ConfirmationStale
            ))
        ));
        // The request capsule retains the profile-child lease used to derive its descriptor.
        // Release this test-only failed-attempt handle before asserting same-child reopen; the
        // journal's retired tombstone keeps identities and the receipt without that capsule.
        drop(initial_request);
        let request = authority.journal_request().expect("journal request retry");
        let duplicate = SourceAuthorityRegistrationRequest {
            request: Arc::clone(&request.request),
            confirmation: request.confirmation.clone(),
        };
        let mut first = client
            .register_source_authority(request)
            .expect("first registration request");
        let receipt = first
            .recv_timeout(Duration::from_secs(2))
            .expect("first registration result")
            .expect("first registration");
        let mut replay = client
            .register_source_authority(duplicate)
            .expect("duplicate registration request");
        let replayed = replay
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate registration result")
            .expect("duplicate registration");
        assert_eq!(replayed, receipt);

        let mut drain = client
            .begin_source_drain(receipt.clone())
            .expect("source drain request");
        drain
            .recv_timeout(Duration::from_secs(2))
            .expect("source drain result")
            .expect("source drain");
        let mut replay_drain = client
            .begin_source_drain(receipt.clone())
            .expect("duplicate source drain request");
        replay_drain
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate source drain result")
            .expect("duplicate source drain");
        let acknowledgement = authority
            .close_database(database)
            .expect("source retirement acknowledgement");
        let mut finalize = client
            .finalize_source_retirement(receipt.clone(), acknowledgement.clone())
            .expect("source finalization request");
        finalize
            .recv_timeout(Duration::from_secs(2))
            .expect("source finalization result")
            .expect("source finalization");
        let mut replay_finalize = client
            .finalize_source_retirement(receipt, acknowledgement)
            .expect("duplicate source finalization request");
        replay_finalize
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate source finalization result")
            .expect("duplicate source finalization");
        drop(authority);

        // The logical registration is gone, but the complete external volume owner/reserve must
        // remain idle so a same-volume source reopen can reuse it without reacquiring a dirty
        // cross-process marker.
        let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "source-authority-catalog",
        )
        .expect("reopen source database on retained volume binding");
        let mut reopened_authority = reopened_database
            .prepare_source_authority()
            .expect("reopened source authority");
        let reopened_request = reopened_authority
            .journal_request()
            .expect("reopened journal request");
        let mut reopened_registration = client
            .register_source_authority(reopened_request)
            .expect("reopened source registration request");
        let reopened_receipt = reopened_registration
            .recv_timeout(Duration::from_secs(2))
            .expect("reopened source registration result")
            .expect("reopened source registration");
        let mut reopened_drain = client
            .begin_source_drain(reopened_receipt.clone())
            .expect("reopened source drain request");
        reopened_drain
            .recv_timeout(Duration::from_secs(2))
            .expect("reopened source drain result")
            .expect("reopened source drain");
        let reopened_ack = reopened_authority
            .close_database(reopened_database)
            .expect("reopened source retirement acknowledgement");
        let mut reopened_finalize = client
            .finalize_source_retirement(reopened_receipt, reopened_ack)
            .expect("reopened source finalization request");
        reopened_finalize
            .recv_timeout(Duration::from_secs(2))
            .expect("reopened source finalization result")
            .expect("reopened source finalization");
        assert!(owner.shutdown());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_rejects_stale_source_proof_before_binding_and_retains_failed_reserve() {
        let fixture = fixture("journal-owner-source-proof-failure");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");

        let source_root = tempfile::tempdir().expect("source root");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "source-proof-failure-catalog",
        )
        .expect("profile-owned source database");
        let authority = database
            .prepare_source_authority()
            .expect("source authority");
        let request = authority.journal_request().expect("journal request");
        let proof = request
            .request
            .accept_confirmation(request.confirmation.clone())
            .expect("source proof");
        let descriptor = request
            .request
            .media_root()
            .try_clone_descriptor()
            .expect("source descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("source facts");
        let volume_owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            facts.identity.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("source volume owner");
        let mut authority_slot = Some(authority);
        let mut abandon_authority = || drop(authority_slot.take());
        let failure = match store.acquire_owned_source_reserve_with_test_hook(
            volume_owner,
            request.request.media_root(),
            &fixture.owner,
            &proof,
            &mut abandon_authority,
        ) {
            Ok(_) => panic!("stale proof must reject before durable binding"),
            Err(failure) => failure,
        };
        assert!(failure.error.to_string().contains("source authority proof"));
        assert_eq!(failure.volume_owner.volume(), &facts.identity);
        assert!(failure.reserve.is_some());
        assert!(failure.volume_owner.revalidate().is_ok());
        let (binding, _) = store
            .test_reserve_bootstrap_snapshot(&facts.identity)
            .expect("source binding snapshot");
        assert!(
            binding.is_none(),
            "proof failure must precede durable binding"
        );
        drop(failure);
        drop(database);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn source_reserve_clean_restart_rebinds_exact_generation() {
        let fixture = fixture("journal-owner-source-clean-rebind");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let source_root = tempfile::tempdir().expect("source root");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "source-clean-rebind-catalog",
        )
        .expect("profile-owned source database");
        let authority = database
            .prepare_source_authority()
            .expect("source authority");
        let request = authority.journal_request().expect("journal request");
        let proof = request
            .request
            .accept_confirmation(request.confirmation.clone())
            .expect("source proof");
        let descriptor = request
            .request
            .media_root()
            .try_clone_descriptor()
            .expect("source descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("source facts");
        let first_owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            facts.identity.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("first source volume owner");
        let first_context_owner = first_owner
            .try_clone_for_context()
            .expect("first source context owner");
        let first_reserve = store
            .acquire_owned_source_reserve(
                first_context_owner,
                request.request.media_root(),
                &fixture.owner,
                &proof,
            )
            .unwrap_or_else(|failure| panic!("first source reserve binding: {}", failure.error));
        drop(first_reserve);
        let (binding_before, counts_before) = store
            .test_reserve_bootstrap_snapshot(&facts.identity)
            .expect("source binding before clean restart");
        let binding_before = binding_before.expect("source binding before clean restart");
        let mut first_owner = first_owner;
        first_owner
            .mark_clean_after_handoff()
            .expect("clean source owner marker");
        drop(first_owner);

        let second_owner =
            super::super::capacity_volume_owner::acquire_fixture_with_clean_marker_for_test(
                &descriptor,
                &registry_path,
                facts.identity.clone(),
                fixture.owner.profile_root_identity(),
                true,
            )
            .expect("clean source owner restart");
        assert_eq!(second_owner.owner_generation(), 2);
        let second_context_owner = second_owner
            .try_clone_for_context()
            .expect("second source context owner");
        let second_reserve = store
            .acquire_owned_source_reserve(
                second_context_owner,
                request.request.media_root(),
                &fixture.owner,
                &proof,
            )
            .unwrap_or_else(|failure| {
                panic!("source reserve generation rebind: {}", failure.error)
            });
        let (binding, _) = store
            .test_reserve_bootstrap_snapshot(&facts.identity)
            .expect("source binding snapshot");
        let binding_after = binding.expect("source binding after clean restart");
        assert_eq!(binding_after.0, binding_before.0);
        assert_eq!(binding_after.1, binding_before.1);
        assert_eq!(binding_after.2, binding_before.2);
        assert_eq!(binding_after.3, binding_before.3 + 1);
        let (_, counts_after) = store
            .test_reserve_bootstrap_snapshot(&facts.identity)
            .expect("source binding counts after clean restart");
        assert_eq!(counts_after, counts_before);
        drop(second_reserve);
        drop(second_owner);
        drop(authority);
        drop(database);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_rejects_incomplete_retained_source_binding_without_reuse() {
        let fixture = fixture("journal-owner-incomplete-source-binding");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let profile_volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        let profile_volume = profile_volume_owner.volume().clone();
        runtime.install_volume_owner(profile_volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);

        let source_root = tempfile::tempdir().expect("source root");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "source-incomplete-binding-catalog",
        )
        .expect("profile-owned source database");
        let authority = database
            .prepare_source_authority()
            .expect("source authority");
        let request = authority.journal_request().expect("journal request");
        let descriptor = request
            .request
            .media_root()
            .try_clone_descriptor()
            .expect("source descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("source facts");
        assert_eq!(facts.identity, profile_volume);
        runtime.source_volumes.insert(
            profile_volume,
            SourceVolumeBinding {
                _current_input: None,
                #[cfg(all(test, unix))]
                _current_before_root_drop: None,
                _current_root: None,
                #[cfg(all(test, unix))]
                _current_after_root_drop: None,
                _current_volume_failure: Default::default(),
                _current_reserve_failure: Default::default(),
                owner: None,
                reserve: None,
                allocation_unit: Some(facts.allocation_unit),
                readiness: SourceVolumeBindingReadiness::Incomplete,
                registrations: BTreeSet::new(),
            },
        );
        let error = match runtime.register_source_authority(request) {
            Ok(_) => panic!("incomplete binding must not be reused"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                receipt: None,
                reason,
            } if reason.contains("incomplete retained binding")
        ));
        drop(authority);
        drop(database);
        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_runtime_rebuild_reuses_live_provider_on_conflicting_snapshot() {
        let fixture = fixture("journal-owner-rebuild-provider-conflict");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("reserve bootstrap");
        runtime.install_reserve(reserve);

        let volume = VolumeIdentity { device: 1 };
        let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging;
        let provider = CapacityProvider::unavailable();
        let snapshot = OrdinaryClaimRebuildSnapshot::for_test(
            vec![RebuildClaim::for_test(
                OperationId::new(),
                crate::operation_journal::AdmissionClaimId::new(),
                volume.clone(),
                class,
                4096,
                None,
                false,
            )],
            CapacityRebuildReadiness::ready(),
        );
        provider
            .install_rebuilt_claims(&snapshot)
            .expect("seed live provider ledger");
        runtime.capacity_provider = Some(provider);

        runtime
            .rebuild_capacity()
            .expect("conflicting rebuild remains recovery-only");
        let live_provider = runtime
            .capacity_provider
            .as_ref()
            .expect("provider retained after rebuild");
        assert_eq!(live_provider.outstanding(&volume, class), 4096);
        assert!(matches!(
            runtime.capacity_rebuild_readiness(),
            CapacityRebuildReadiness::ReconciliationRequired { diagnostics }
                if diagnostics.iter().any(|diagnostic| diagnostic.contains("rejected"))
        ));
        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_runtime_does_not_read_or_install_after_physical_proof_failure() {
        let fixture = fixture("journal-owner-rebuild-proof-failure");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("reserve bootstrap");
        runtime.install_reserve(reserve);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&reserve_path)
            .expect("open reserve for proof-failure fixture");

        let error = runtime
            .rebuild_capacity()
            .expect_err("short reserve must stop rebuild before store read");
        assert!(error.contains("physical reserve proof"));
        assert!(runtime.capacity_provider.is_none());
        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_runtime_retains_profile_and_volume_ownership_until_shutdown() {
        let fixture = fixture("journal-owner-runtime-lifetime");
        let registry_path = fixture_volume_registry(&fixture);
        let Fixture {
            owner: profile_owner,
            _profile,
            _config,
            _base,
            _runtime,
        } = fixture;
        let profile_identity = profile_owner.profile_root_identity().to_owned();
        let profile_root = profile_owner.profile_root().to_path_buf();
        let store = OperationJournalStore::open(&profile_owner).expect("journal store");
        drop(profile_owner);

        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            registry_path.clone(),
        )
        .expect("journal owner");
        wait_until_available(&owner);
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(crate::app_dirs::ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));

        let reserve_path = profile_root
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("open reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let contention = match super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            facts.identity.clone(),
            &profile_identity,
        ) {
            Ok(_) => panic!("volume owner must remain held while worker is available"),
            Err(error) => error,
        };
        assert!(matches!(
            contention,
            super::super::capacity_volume_owner::VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));

        assert!(owner.shutdown());
        let replacement = WritableProfileGuard::acquire_current().expect("profile released");
        let released = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            facts.identity,
            &profile_identity,
        )
        .expect("volume owner released after shutdown");
        drop(released);
        drop(replacement);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_runtime_drop_observes_normal_and_unwind_teardown_boundaries() {
        let fixture = fixture("journal-owner-runtime-drop-boundary");
        let registry_path = fixture_volume_registry(&fixture);
        let Fixture {
            owner: profile_owner,
            _profile,
            _config,
            _base,
            _runtime,
        } = fixture;
        let profile_identity = profile_owner.profile_root_identity().to_owned();
        let profile_root = profile_owner.profile_root().to_path_buf();
        let store = OperationJournalStore::open(&profile_owner).expect("journal store");
        drop(profile_owner);

        let install_observed_runtime = |store: OperationJournalStore| {
            let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
            let volume_owner = runtime
                .store()
                .acquire_profile_journal_volume_owner(Some(&registry_path))
                .expect("volume owner");
            runtime.install_volume_owner(volume_owner);
            let reserve = runtime
                .store()
                .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
                .expect("reserve bootstrap");
            runtime.install_reserve(reserve);
            let reserve_path = profile_root
                .join("operation_journal")
                .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
            let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
            let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
            let observer = Arc::new(Mutex::new(None));
            runtime.install_drop_observer(
                Arc::clone(&observer),
                OwnerRuntimeDropProbe {
                    descriptor,
                    registry_path: registry_path.clone(),
                    volume: facts.identity,
                    profile_identity: profile_identity.clone(),
                },
            );
            (runtime, observer)
        };

        let (runtime, normal_observer) = install_observed_runtime(store);
        drop(runtime);
        let normal = normal_observer
            .lock()
            .expect("normal observer")
            .clone()
            .expect("normal drop observation");
        assert!(normal.profile_held_after_store_drop);
        assert!(normal.volume_held_after_store_drop);
        assert!(normal.profile_available_after_teardown);
        assert!(normal.volume_available_after_teardown);

        let restarted_store = OperationJournalStore::open(
            &WritableProfileGuard::acquire_current().expect("profile for restart"),
        )
        .expect("restart store");
        let (runtime, unwind_observer) = install_observed_runtime(restarted_store);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _runtime = runtime;
            panic!("owner runtime unwind fixture");
        }));
        assert!(result.is_err());
        let unwind = unwind_observer
            .lock()
            .expect("unwind observer")
            .clone()
            .expect("unwind drop observation");
        assert!(unwind.profile_held_after_store_drop);
        assert!(unwind.volume_held_after_store_drop);
        assert!(unwind.profile_available_after_teardown);
        assert!(unwind.volume_available_after_teardown);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_reopens_explained_spent_partial_profile_journal_reserve() {
        let fixture = fixture("journal-owner-partial-reserve-reopen");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let operation_id = OperationId::new();
        store
            .insert_intent(&intent_record(operation_id))
            .expect("durable seed intent");
        store
            .test_seed_spent_profile_journal_reserve(
                &registry_path,
                operation_id,
                uuid::Uuid::new_v4(),
                8192,
                4096,
                4096,
            )
            .expect("seed spent reserve");
        drop(store);

        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let reserve_descriptor = fs::File::open(&reserve_path).expect("open partial reserve");
        let volume = descriptor_capacity_facts(&reserve_descriptor)
            .expect("partial reserve capacity facts")
            .identity;
        let (binding_before, counts_before) = reopened_store
            .test_reserve_bootstrap_snapshot(&volume)
            .expect("partial bootstrap snapshot");
        assert!(binding_before.is_some());
        assert_eq!(counts_before, (1, 1, 1, 2));
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            reopened_store,
            registry_path,
        )
        .expect("reopen owner");
        wait_until_available(&owner);
        assert!(owner.shutdown());

        let metadata = fs::metadata(&reserve_path).expect("partial reserve after reopen");
        assert!(metadata.len() < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR);
        assert!(metadata.len() >= 4096);
        let final_store = OperationJournalStore::open(&fixture.owner).expect("final store");
        let (binding_after, counts_after) = final_store
            .test_reserve_bootstrap_snapshot(&volume)
            .expect("reopened partial bootstrap snapshot");
        assert_eq!(binding_after, binding_before);
        assert_eq!(counts_after, counts_before);
        drop(final_store);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_rejects_existing_unbound_reserve_without_replacement() {
        let fixture = fixture("journal-owner-unbound-reserve");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        fs::write(&reserve_path, b"retained-unbound-reserve").expect("seed unbound reserve");
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            registry_path.clone(),
        )
        .expect("journal owner");
        let reason = wait_until_unavailable(&owner);
        assert!(reason.contains("reserve bootstrap"));
        assert!(reason.contains("child already exists"));
        assert_eq!(
            fs::read(&reserve_path).expect("retained reserve"),
            b"retained-unbound-reserve"
        );
        assert!(!owner.shutdown());

        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let reserve_descriptor = fs::File::open(&reserve_path).expect("open unbound reserve");
        let volume = descriptor_capacity_facts(&reserve_descriptor)
            .expect("unbound reserve capacity facts")
            .identity;
        let (binding, counts) = reopened_store
            .test_reserve_bootstrap_snapshot(&volume)
            .expect("unbound bootstrap snapshot");
        assert!(binding.is_none());
        assert_eq!(counts, (0, 0, 0, 0));
        drop(reopened_store);

        let retry_store = OperationJournalStore::open(&fixture.owner).expect("retry store");
        let mut retry_owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(retry_store, registry_path)
                .expect("retry journal owner");
        let retry_reason = wait_until_unavailable(&retry_owner);
        assert!(retry_reason.contains("child already exists"));
        assert!(!retry_owner.shutdown());
        drop(fixture);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn owner_rejects_unsupported_reserve_before_namespace_effects() {
        let fixture = fixture("journal-owner-unsupported-reserve");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let registry_path = fixture.owner.profile_root().join("volume-owner-registry");
        fs::create_dir(&registry_path).expect("fixture volume registry");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        let reason = wait_until_unavailable(&owner);
        assert!(reason.contains("reserve bootstrap"), "{reason}");
        assert!(!reserve_path.exists());
        assert!(!owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn owner_reads_full_progress_and_combined_recovery_through_one_queue() {
        let fixture = fixture("journal-owner-combined-recovery");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let legacy = intent_record(OperationId::new());
        store.insert_intent(&legacy).expect("legacy seed");
        let (snapshot, _) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        let full = StoredFullProgressRecord::new(snapshot).expect("full seed");
        let full_id = full.snapshot().intent().operation_id();
        assert_eq!(
            store
                .persist_full(&full, None)
                .expect("full seed")
                .sequence(),
            2
        );

        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();

        let mut full_request = client.get_full(full_id).expect("full lookup admission");
        let observed = full_request
            .recv_timeout(Duration::from_secs(2))
            .expect("full lookup result")
            .expect("full lookup worker")
            .expect("full observation");
        assert_eq!(observed, full);

        let mut recovery_request = client
            .scan_coordinator_recovery_page(None)
            .expect("combined recovery admission");
        let page = recovery_request
            .recv_timeout(Duration::from_secs(2))
            .expect("combined recovery result")
            .expect("combined recovery worker");
        assert_eq!(page.entries().len(), 2);
        assert_eq!(page.entries()[0].sequence(), 1);
        assert_eq!(page.entries()[1].sequence(), 2);
        assert!(matches!(
            page.entries()[1].outcome(),
            super::super::store::recovery::CoordinatorRecoveryRowOutcome::Consistent(
                super::super::store::recovery::CoordinatorRecoveryRecord::FullProgress(observed)
            ) if observed == &full
        ));
        let mut status_request = client
            .scan_recovery_status_page(None)
            .expect("status admission");
        let status = status_request
            .recv_timeout(Duration::from_secs(2))
            .expect("status result")
            .expect("status worker");
        assert_eq!(status.entries().len(), 2);
        assert_eq!(
            status.entries()[0].operation_id(),
            Some(legacy.operation_id)
        );
        assert_eq!(status.entries()[1].operation_id(), Some(full_id));
        assert_eq!(
            status.entries()[1].outcome(),
            super::super::RecoveryStatusOutcome::FullProgress {
                phase: full.snapshot().phase(),
                disposition: full.snapshot().disposition(),
            }
        );
        assert!(status.is_done());
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn owner_persists_full_progress_with_cas_replay_and_observation_reopen() {
        let fixture = fixture("journal-owner-full-progress-write");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (initial_snapshot, progressed_snapshot) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        let initial = StoredFullProgressRecord::new(initial_snapshot).expect("initial full record");
        let progressed =
            StoredFullProgressRecord::new(progressed_snapshot).expect("progressed full record");
        let operation_id = initial.snapshot().intent().operation_id();
        assert_eq!(progressed.snapshot().intent().operation_id(), operation_id);

        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();

        let mut insert = client
            .persist_full(initial.clone(), None)
            .expect("initial full observation queues");
        assert_eq!(
            insert
                .recv_timeout(Duration::from_secs(2))
                .expect("initial full result")
                .expect("initial full worker"),
            FullProgressWriteOutcome::Applied { sequence: 1 }
        );

        let mut initial_replay = client
            .persist_full(initial.clone(), None)
            .expect("initial replay queues");
        assert_eq!(
            initial_replay
                .recv_timeout(Duration::from_secs(2))
                .expect("initial replay result")
                .expect("initial replay worker"),
            FullProgressWriteOutcome::Replay { sequence: 1 }
        );

        let mut update = client
            .persist_full(progressed.clone(), Some(1))
            .expect("full observation update queues");
        assert_eq!(
            update
                .recv_timeout(Duration::from_secs(2))
                .expect("full update result")
                .expect("full update worker"),
            FullProgressWriteOutcome::Applied { sequence: 2 }
        );

        let mut update_replay = client
            .persist_full(progressed.clone(), Some(1))
            .expect("stale acknowledgement replay queues");
        assert_eq!(
            update_replay
                .recv_timeout(Duration::from_secs(2))
                .expect("stale replay result")
                .expect("stale replay worker"),
            FullProgressWriteOutcome::Replay { sequence: 2 }
        );

        let mut stale_update = client
            .persist_full(initial.clone(), Some(1))
            .expect("stale conflict queues");
        assert!(matches!(
            stale_update
                .recv_timeout(Duration::from_secs(2))
                .expect("stale conflict result")
                .expect_err("changed stale record must conflict"),
            OperationJournalOwnerError::Store(
                OperationJournalStoreError::FullProgressSequenceConflict {
                    expected: Some(1),
                    actual: Some(2),
                    ..
                }
            )
        ));

        let mut observed = client
            .get_full(operation_id)
            .expect("full observation lookup queues");
        let observed = observed
            .recv_timeout(Duration::from_secs(2))
            .expect("full observation lookup result")
            .expect("full observation lookup worker")
            .expect("full observation remains present");
        assert_eq!(observed, progressed);
        let rehydrated = observed
            .snapshot()
            .rehydrate_observation()
            .expect("stored observation rehydrates");
        let recaptured = rehydrated
            .capture_full_progress_snapshot(observed.snapshot().intent())
            .expect("rehydrated observation recaptures");
        assert_eq!(&recaptured, observed.snapshot());
        assert!(owner.shutdown());

        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let mut reopened_owner =
            OperationJournalOwner::start_without_capacity_for_test(reopened_store)
                .expect("reopen owner");
        wait_until_available(&reopened_owner);
        let mut reopened = reopened_owner
            .client()
            .get_full(operation_id)
            .expect("reopened full lookup queues");
        assert_eq!(
            reopened
                .recv_timeout(Duration::from_secs(2))
                .expect("reopened full lookup result")
                .expect("reopened full lookup worker")
                .expect("reopened full observation"),
            progressed
        );
        assert!(reopened_owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn owner_reopens_each_guarded_terminal_observation_without_authority_restoration() {
        let fixture = fixture("journal-owner-guarded-terminals-reopen");
        let snapshots =
            crate::operation_journal::work_progress::test_guarded_terminal_store_snapshots();
        assert_eq!(snapshots.len(), 4);
        let mut records = snapshots
            .into_iter()
            .map(|snapshot| StoredFullProgressRecord::new(snapshot).expect("terminal record"))
            .collect::<Vec<_>>();
        records.push(StoredFullProgressRecord::new(
            crate::operation_journal::work_progress::test_guarded_terminal_origin_restoration_snapshot(),
        ).expect("origin restoration terminal record"));
        assert_eq!(records.len(), 5);
        let operation_ids = records
            .iter()
            .map(|record| record.snapshot().intent().operation_id())
            .collect::<Vec<_>>();

        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        for (index, record) in records.iter().cloned().enumerate() {
            let mut request = owner
                .client()
                .persist_full(record, None)
                .expect("terminal observation queues");
            assert_eq!(
                request
                    .recv_timeout(Duration::from_secs(2))
                    .expect("terminal observation result")
                    .expect("terminal observation worker"),
                FullProgressWriteOutcome::Applied {
                    sequence: (index + 1) as i64,
                }
            );
        }
        assert!(owner.shutdown());

        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let mut reopened_owner =
            OperationJournalOwner::start_without_capacity_for_test(reopened_store)
                .expect("reopen owner");
        wait_until_available(&reopened_owner);
        for (operation_id, expected) in operation_ids.into_iter().zip(&records) {
            let mut request = reopened_owner
                .client()
                .get_full(operation_id)
                .expect("reopened terminal lookup queues");
            let observed = request
                .recv_timeout(Duration::from_secs(2))
                .expect("reopened terminal lookup result")
                .expect("reopened terminal lookup worker")
                .expect("reopened terminal observation");
            assert_eq!(&observed, expected);
            let snapshot = observed.snapshot();
            assert!(snapshot.terminal_outcome_observation().is_some());
            assert!(snapshot.claim_settlement().is_some());
            if matches!(
                snapshot.disposition(),
                super::super::full_status::FullProgressDisposition::FailedPreservingData
            ) {
                assert!(snapshot.retry_observation().is_some());
            }
            if matches!(
                snapshot.disposition(),
                super::super::full_status::FullProgressDisposition::FailedDataLossRisk
            ) {
                assert!(snapshot.audit_observation().is_some());
            }
            let rehydrated = snapshot
                .rehydrate_observation()
                .expect("reopened terminal observation rehydrates");
            let recaptured = rehydrated
                .capture_full_progress_snapshot(snapshot.intent())
                .expect("reopened terminal observation recaptures");
            assert_eq!(&recaptured, snapshot);
        }
        assert!(reopened_owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn owner_full_progress_commit_survives_dropped_ack() {
        let fixture = fixture("journal-owner-full-progress-drop");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (initial_snapshot, progressed_snapshot) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        let initial = StoredFullProgressRecord::new(initial_snapshot).expect("initial full record");
        let progressed =
            StoredFullProgressRecord::new(progressed_snapshot).expect("progressed full record");
        let operation_id = initial.snapshot().intent().operation_id();

        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();
        let dropped = client
            .persist_full(initial, None)
            .expect("full observation queues before dropped ack");
        drop(dropped);

        let mut update = client
            .persist_full(progressed.clone(), Some(1))
            .expect("full update queues after dropped ack");
        assert_eq!(
            update
                .recv_timeout(Duration::from_secs(2))
                .expect("full update result")
                .expect("full update worker"),
            FullProgressWriteOutcome::Applied { sequence: 2 }
        );
        let mut observed = client
            .get_full(operation_id)
            .expect("full observation lookup queues");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(2))
                .expect("full observation lookup result")
                .expect("full observation lookup worker")
                .expect("full observation remains present"),
            progressed
        );
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn owner_rejects_oversized_full_progress_before_queue_or_storage() {
        let fixture = fixture("journal-owner-full-progress-bounds");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let oversized =
            crate::operation_journal::work_progress::test_oversized_full_progress_store_record();
        let operation_id = oversized.snapshot().intent().operation_id();

        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();
        assert!(matches!(
            client.persist_full(oversized, None),
            Err(OperationJournalOwnerQueueError::RecordTooLarge { .. })
        ));

        let mut lookup = client
            .get_full(operation_id)
            .expect("full lookup after rejected input queues");
        assert!(
            lookup
                .recv_timeout(Duration::from_secs(2))
                .expect("full lookup result")
                .expect("full lookup worker")
                .is_none()
        );
        let mut recovery = client
            .scan_coordinator_recovery_page(None)
            .expect("recovery lookup after rejected input queues");
        let page = recovery
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery result")
            .expect("recovery worker");
        assert!(page.entries().is_empty());
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn full_admission_fails_closed_without_mutating_either_journal_family() {
        let fixture = fixture("journal-owner-full-admission");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let legacy = intent_record(OperationId::new());
        store.insert_intent(&legacy).expect("legacy seed");
        let (snapshot, _) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        let full = StoredFullProgressRecord::new(snapshot).expect("full seed");
        store.persist_full(&full, None).expect("full seed");
        let before = store
            .scan_coordinator_recovery_page(None)
            .expect("initial recovery page");
        let before_sequences: Vec<_> = before
            .entries()
            .iter()
            .map(|entry| entry.sequence())
            .collect();

        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let (args, participants) = valid_full_admission_request();
        let mut request = owner
            .client()
            .admit_full(args, participants)
            .expect("admission request queues");
        let result = request
            .recv_timeout(Duration::from_secs(2))
            .expect("admission result")
            .expect_err("provider remains fail-closed");
        assert!(
            matches!(
                result,
                OperationJournalOwnerError::CapacityProviderUnavailable
            ),
            "unexpected admission result: {result:?}"
        );
        assert!(owner.shutdown());

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen journal");
        let after = reopened
            .scan_coordinator_recovery_page(None)
            .expect("final recovery page");
        let after_sequences: Vec<_> = after
            .entries()
            .iter()
            .map(|entry| entry.sequence())
            .collect();
        assert_eq!(after_sequences, before_sequences);
        assert!(matches!(
            after.entries()[0].outcome(),
            super::super::store::recovery::CoordinatorRecoveryRowOutcome::Consistent(
                super::super::store::recovery::CoordinatorRecoveryRecord::Legacy(_)
            )
        ));
        assert!(matches!(
            after.entries()[1].outcome(),
            super::super::store::recovery::CoordinatorRecoveryRowOutcome::Consistent(
                super::super::store::recovery::CoordinatorRecoveryRecord::FullProgress(_)
            )
        ));
        drop(fixture);
    }

    #[test]
    fn full_admission_rejects_invalid_typed_input_before_queueing() {
        let (_fixture, mut owner) = open_owner("journal-owner-full-admission-input");
        let client = owner.client();
        let (args, participants) = valid_full_admission_request();
        let invalid_participants: ParticipantSet = serde_json::from_value(serde_json::json!({
            "descriptors": [],
            "checkpoints": []
        }))
        .expect("decode invalid participant fixture");
        assert!(matches!(
            client.admit_full(args.clone(), invalid_participants),
            Err(OperationJournalOwnerQueueError::InvalidInput { .. })
        ));

        let UniversalCommandArgs::Copy(copy_args) = args else {
            panic!("copy fixture");
        };
        let mismatched_source = CopySourceExpectation::new(8, "owner-file", 10, 11)
            .expect("mismatched source expectation shape");
        let invalid_args = CopyIntentArgs::new(
            copy_args.source().clone(),
            copy_args.destination().clone(),
            mismatched_source,
            copy_args.collision_policy(),
            copy_args.inherited_metadata_reference().map(String::from),
        )
        .expect("invalid participant binding still has valid shape");
        assert!(matches!(
            client.admit_full(UniversalCommandArgs::Copy(invalid_args), participants),
            Err(OperationJournalOwnerQueueError::InvalidInput { .. })
        ));
        assert!(owner.shutdown());
    }

    #[test]
    fn full_admission_oversized_input_is_rejected_before_queue_credits() {
        let (_fixture, mut owner) = open_owner("journal-owner-full-admission-bounds");
        let client = owner.client();
        let (args, participants) = valid_full_admission_request();
        let mut value = serde_json::to_value(&participants).expect("participants JSON");
        value["checkpoints"][0]["diagnostic_code"] =
            serde_json::Value::String("x".repeat(MAX_RECORD_BYTES + 1));
        let oversized: ParticipantSet =
            serde_json::from_value(value).expect("decode oversized participant fixture");
        assert!(matches!(
            client.admit_full(args, oversized),
            Err(OperationJournalOwnerQueueError::RecordTooLarge { .. })
        ));
        assert!(owner.shutdown());
    }

    #[test]
    fn full_admission_reports_store_guard_failure_before_provider_result() {
        let fixture = fixture("journal-owner-full-admission-guard");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_without_capacity_for_test(store).expect("journal owner");
        wait_until_available(&owner);
        let sidecar = fixture
            .owner
            .profile_root()
            .join(super::super::store::JOURNAL_DATABASE_ROOT_NAME)
            .join(format!(
                "{}-wal",
                super::super::store::JOURNAL_DATABASE_FILE_NAME
            ));
        fs::write(&sidecar, b"unexpected owner admission sidecar").expect("inject sidecar");

        let (args, participants) = valid_full_admission_request();
        let mut request = owner
            .client()
            .admit_full(args, participants)
            .expect("admission request queues");
        let result = request
            .recv_timeout(Duration::from_secs(2))
            .expect("admission result")
            .expect_err("sidecar must fail the store guard");
        assert!(matches!(result, OperationJournalOwnerError::Store(_)));
        assert!(format!("{result}").contains("sidecar"));

        fs::remove_file(sidecar).expect("remove injected sidecar");
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[test]
    fn full_admission_dropped_request_releases_shared_owner_permits() {
        let fixture = fixture("journal-owner-full-admission-drop");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let (args, participants) = valid_full_admission_request();
        let mut first = client
            .admit_full(args.clone(), participants.clone())
            .expect("first admission request");
        ready.wait();
        let dropped = client
            .admit_full(args.clone(), participants.clone())
            .expect("second admission request");
        drop(dropped);
        release.wait();
        let first_result = first
            .recv_timeout(Duration::from_secs(2))
            .expect("first admission result")
            .expect_err("provider remains fail-closed");
        assert!(
            matches!(
                first_result,
                OperationJournalOwnerError::CapacityProviderUnavailable
            ),
            "unexpected first admission result: {first_result:?}"
        );
        let mut replacement = client
            .admit_full(args, participants)
            .expect("replacement request after dropped request");
        let replacement_result = replacement
            .recv_timeout(Duration::from_secs(2))
            .expect("replacement result")
            .expect_err("provider remains fail-closed");
        assert!(
            matches!(
                replacement_result,
                OperationJournalOwnerError::CapacityProviderUnavailable
            ),
            "unexpected replacement result: {replacement_result:?}"
        );
        assert!(owner.shutdown());
        let (closed_args, closed_participants) = valid_full_admission_request();
        assert!(matches!(
            client.admit_full(closed_args, closed_participants),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
        drop(fixture);
    }

    fn intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 1,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "owner-test"
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

    fn v5_copy_record(operation_id: OperationId) -> (StoredOperationRecord, CopyIntentArgs) {
        let source_id = SourceId::from_string("owner-source");
        let source_participant_id = crate::operation_journal::ParticipantId::new();
        let filesystem_participant_id = crate::operation_journal::ParticipantId::new();
        let source_fence =
            StoredSourceFence::new(source_id.clone(), "owner-root", "owner-database", 1, 7, 1)
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
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant_id,
                    ParticipantCheckpointState::Pending,
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
            "operation.status.owner-copy",
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
            CopySourceExpectation::new(7, "owner-file", 10, 11).expect("source expectation");
        let command_args = CopyIntentArgs::new(
            source,
            destination,
            expected_source,
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/owner-entry")),
        )
        .expect("copy arguments");
        let capacity_plan = DurableCapacityPlan {
            volumes: vec![crate::durable_capacity::DurableVolumeCapacity {
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
            label: String::from("owner copy"),
        };
        (
            StoredOperationRecord::new_v5_copy(
                operation_id,
                intent,
                universal_intent,
                command_args.clone(),
                capacity_plan,
                50,
            ),
            command_args,
        )
    }

    fn valid_full_admission_request() -> (UniversalCommandArgs, ParticipantSet) {
        let (snapshot, _) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        (
            snapshot.intent().args().clone(),
            snapshot.intent().participants().clone(),
        )
    }

    fn prepared_copy_input(
        class: crate::operation_journal::full_capacity::FullCapacityAllocationClass,
        volume: VolumeIdentity,
        allocation_unit: u64,
    ) -> PreparedAdmissionInput {
        prepared_copy_input_with_claim_id(
            class,
            volume,
            allocation_unit,
            crate::operation_journal::AdmissionClaimId::new(),
        )
    }

    fn prepared_copy_input_with_claim_id(
        class: crate::operation_journal::full_capacity::FullCapacityAllocationClass,
        volume: VolumeIdentity,
        allocation_unit: u64,
        claim_id: crate::operation_journal::AdmissionClaimId,
    ) -> PreparedAdmissionInput {
        let (args, participants) = valid_full_admission_request();
        let UniversalCommandArgs::Copy(copy_args) = args else {
            panic!("copy admission fixture")
        };
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.prepared-owner-test",
            participants.clone(),
        )
        .expect("prepared copy universal intent");
        let allocation = crate::operation_journal::FullCapacityAllocation::new(
            volume.clone(),
            allocation_unit,
            class,
            allocation_unit,
            allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("prepared allocation");
        let plan = crate::operation_journal::FullCapacityPlan::new(vec![allocation])
            .expect("prepared plan");
        let claim =
            AdmissionClaim::new(claim_id, volume, class, allocation_unit).expect("prepared claim");
        let claims = AdmissionClaimInventory::new(vec![claim]).expect("prepared claims");
        PreparedAdmissionInput::for_test(
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::FileMutation,
                label: String::from("prepared owner test"),
            },
            universal_intent,
            UniversalCommandArgs::Copy(copy_args),
            participants,
            plan,
            claims,
        )
    }

    #[cfg(target_os = "macos")]
    fn prepared_copy_all_classes_input(
        volume: VolumeIdentity,
        allocation_unit: u64,
    ) -> PreparedAdmissionInput {
        let (args, participants) = valid_full_admission_request();
        let UniversalCommandArgs::Copy(copy_args) = args else {
            panic!("copy admission fixture")
        };
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.prepared-owner-full-classes-test",
            participants.clone(),
        )
        .expect("prepared full-class universal intent");
        let classes = [
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::JournalRecord,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::SourceDatabaseCommit,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::SourceWalShm,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::GlobalDatabaseCommit,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::GlobalWalShm,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::ExistingDestination,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::ExistingRecoveryPayload,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload,
        ];
        let allocations = classes
            .iter()
            .copied()
            .map(|class| {
                crate::operation_journal::FullCapacityAllocation::new(
                    volume.clone(),
                    allocation_unit,
                    class,
                    allocation_unit,
                    allocation_unit,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .expect("prepared full-class allocation")
            })
            .collect();
        let plan = crate::operation_journal::FullCapacityPlan::new(allocations)
            .expect("prepared full-class plan");
        let claims = AdmissionClaimInventory::new(
            classes
                .iter()
                .copied()
                .map(|class| {
                    AdmissionClaim::new(
                        crate::operation_journal::AdmissionClaimId::new(),
                        volume.clone(),
                        class,
                        allocation_unit,
                    )
                    .expect("prepared full-class claim")
                })
                .collect(),
        )
        .expect("prepared full-class claims");
        PreparedAdmissionInput::for_test(
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::FileMutation,
                label: String::from("prepared owner full classes test"),
            },
            universal_intent,
            UniversalCommandArgs::Copy(copy_args),
            participants,
            plan,
            claims,
        )
    }

    #[test]
    fn live_chunk_effect_spec_rejects_nil_and_unbounded_inputs() {
        let target = RootRegistrationRef::synthetic_for_test();
        let fence = SourceFence::new(
            SourceId::from_string(String::from("source")),
            "root",
            "database",
            1,
            None,
            1,
        )
        .expect("source fence");
        let make = |effect_id, output, fences, offset, logical_bytes, payload: &[u8]| {
            LiveChunkEffectSpec::new(
                effect_id,
                output,
                target,
                fences,
                offset,
                logical_bytes,
                payload,
            )
        };
        assert!(
            make(
                EffectId::from_uuid(Uuid::nil()),
                OutputIdentity::new(),
                vec![fence.clone()],
                0,
                1,
                b"a",
            )
            .is_err()
        );
        assert!(
            make(
                EffectId::new(),
                OutputIdentity::from_uuid(Uuid::nil()),
                vec![fence.clone()],
                0,
                1,
                b"a",
            )
            .is_err()
        );
        assert!(
            make(
                EffectId::new(),
                OutputIdentity::new(),
                vec![fence.clone(), fence.clone()],
                0,
                1,
                b"a",
            )
            .is_err()
        );
        assert!(
            make(
                EffectId::new(),
                OutputIdentity::new(),
                vec![fence.clone()],
                u64::MAX,
                2,
                b"a",
            )
            .is_err()
        );
        assert!(
            make(
                EffectId::new(),
                OutputIdentity::new(),
                vec![fence],
                0,
                1,
                b"ab",
            )
            .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_chunk_grant_crosses_to_file_owner_once_after_dropped_reply() {
        native_live_chunk_and_staging_issuer(None, None, None, None, None, false);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publication_issuer_checkpoint_and_grant_uncertainty_close_original_gate() {
        use super::super::store::InsertTestBoundary;
        for nth in [1, 2] {
            for boundary in [InsertTestBoundary::Commit, InsertTestBoundary::AfterCommit] {
                native_live_chunk_and_staging_issuer(
                    Some((nth, boundary)),
                    None,
                    None,
                    None,
                    None,
                    false,
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_preissued_publication_grant_refuses_original_drain_and_unavailable() {
        for unavailable in [false, true] {
            native_live_chunk_and_staging_issuer(None, Some(unavailable), None, None, None, false);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publication_issuer_refuses_stale_original_targets() {
        for kind in 0..3 {
            native_live_chunk_and_staging_issuer(None, None, Some(kind), None, None, false);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publication_issuer_cancellation_fences_original_pending_and_unsettled_grants() {
        for entered in [false, true] {
            native_live_chunk_and_staging_issuer(None, None, None, Some(entered), None, false);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publication_issuer_chunk_release_fences_original_unused_and_unsettled_grants() {
        for entered in [false, true] {
            native_live_chunk_and_staging_issuer(None, None, None, None, Some(entered), false);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publication_original_handoff_retains_lost_reply_and_tombstone_grant() {
        native_live_chunk_and_staging_issuer(None, None, None, None, None, true);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    #[ignore = "Requires native macOS original registered source, reserve and volume resources; OPT-2325"]
    fn native_original_completion_handoff_requires_macos_resources() {
        native_live_chunk_and_staging_issuer(None, None, None, None, None, true);
    }

    #[cfg(unix)]
    fn native_live_chunk_and_staging_issuer(
        fault: Option<(usize, super::super::store::InsertTestBoundary)>,
        close_before_entry: Option<bool>,
        stale_target: Option<u8>,
        cancel_after_entry: Option<bool>,
        release_after_entry: Option<bool>,
        publication_handoff: bool,
    ) {
        use super::super::store::{InsertTestBoundary, InsertTestDirective};
        use crate::durable_capacity::descriptor_capacity_facts;
        use crate::file_operation_owner::{EffectOutcome, FileOperationOwner};
        use crate::sample_sources::db::SourceDatabase;
        use std::time::Instant;
        let fixture = fixture("journal-owner-live-chunk-grant");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let armed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if let Some((_, selected)) = fault {
            let armed = armed.clone();
            store.set_insert_test_hook(move |boundary| {
                if boundary != selected || armed.load(Ordering::Acquire) == 0 {
                    return InsertTestDirective::Continue;
                }
                if armed.fetch_sub(1, Ordering::AcqRel) != 1 {
                    return InsertTestDirective::Continue;
                }
                let error = OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: "native original issuer durable-outcome fault".into(),
                };
                if boundary == InsertTestBoundary::Commit {
                    InsertTestDirective::ExecuteThenFail(error)
                } else {
                    InsertTestDirective::Fail(error)
                }
            });
        }
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let client = owner.client();

        let source_root = tempfile::tempdir().expect("source root");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &fixture.owner,
            "live-chunk-grant-source",
        )
        .expect("source database");
        let authority = database
            .prepare_source_authority()
            .expect("source authority");
        let journal_request = authority.journal_request().expect("source journal request");
        let source_request = Arc::clone(&journal_request.request);
        let (_request, guard) = authority.split();
        let mut register = client
            .register_source_authority(journal_request)
            .expect("source registration queues");
        let registration = register
            .recv_timeout(Duration::from_secs(2))
            .expect("source registration result")
            .expect("source registration worker");
        let lease_epoch = u64::try_from(registration.lease_identity.epoch())
            .expect("positive source lease epoch");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let mut input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        );
        let prior_intent = input.universal_intent.clone();
        let descriptors = input
            .participants
            .descriptors()
            .iter()
            .map(|descriptor| {
                let source_fence = descriptor.source_fence().map(|stored| {
                    StoredSourceFence::new(
                        stored.source_id().clone(),
                        source_request.media_root().identity(),
                        source_request.database_file_identity(),
                        stored.lifecycle_generation(),
                        stored.source_revision(),
                        lease_epoch,
                    )
                    .expect("participant source fence")
                });
                ParticipantDescriptor::new(
                    descriptor.id(),
                    descriptor.kind(),
                    descriptor.requirement(),
                    source_fence,
                    descriptor.artifact_key().map(str::to_owned),
                )
                .expect("participant descriptor")
            })
            .collect();
        let participants =
            ParticipantSet::new(descriptors, input.participants.checkpoints().to_vec())
                .expect("live source participants");
        let stored_fence = participants
            .descriptors()
            .iter()
            .filter_map(ParticipantDescriptor::source_fence)
            .next()
            .expect("source participant");
        let fence = SourceFence::new(
            stored_fence.source_id().clone(),
            stored_fence.root_identity(),
            stored_fence.database_identity(),
            stored_fence.lifecycle_generation(),
            Some(stored_fence.source_revision()),
            stored_fence.lease_epoch(),
        )
        .expect("source fence");
        input.universal_intent = UniversalIntent::new(
            prior_intent.parent_operation_id(),
            prior_intent.command_kind(),
            prior_intent.user_status_key(),
            participants.clone(),
        )
        .expect("live source universal intent");
        input.participants = participants;
        let mut admit = client.admit_prepared(input).expect("operation queues");
        let operation_id = admit
            .recv_timeout(Duration::from_secs(2))
            .expect("operation result")
            .expect("operation worker");
        let mut lookup = client.get(operation_id).expect("operation lookup queues");
        let operation = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("operation lookup result")
            .expect("operation lookup worker")
            .expect("operation record");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity,
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("claim"),
            participant_id,
            String::from("opaque-output-key"),
            0,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("chunk request");
        let mut claim_result = client.claim_chunk(claim.clone()).expect("claim queues");
        assert_eq!(
            claim_result
                .recv_timeout(Duration::from_secs(2))
                .expect("claim result")
                .expect("claim worker")
                .state(),
            ChunkClaimState::Claimed
        );
        let file_owner = FileOperationOwner::start().expect("file owner");
        let source_root_registration = file_owner
            .register_source_root(Arc::clone(&source_request), &fixture.owner, fence.clone())
            .expect("journal-registered source root transfers to file owner");
        let target = file_owner
            .register_live_chunk_root_for_test()
            .expect("registered staging root");
        let effect_id = EffectId::new();
        let output = OutputIdentity::new();
        let wrong_fence = SourceFence::new(
            fence.source_id().clone(),
            "unregistered-root",
            fence.database_identity(),
            fence.lifecycle_generation(),
            fence.source_revision(),
            lease_epoch,
        )
        .expect("wrong source fence observation");
        assert!(
            file_owner
                .register_source_root(
                    Arc::clone(&source_request),
                    &fixture.owner,
                    wrong_fence.clone(),
                )
                .is_err()
        );
        let wrong_spec =
            LiveChunkEffectSpec::new(effect_id, output, target, vec![wrong_fence], 0, 3, b"abc")
                .expect("bounded wrong observation");
        let mut denied = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("source pin for denial"),
                wrong_spec,
            )
            .expect("wrong issue queues");
        assert!(
            denied
                .recv_timeout(Duration::from_secs(2))
                .expect("wrong issue result")
                .is_err()
        );
        let stale_generation = SourceFence::new(
            fence.source_id().clone(),
            fence.root_identity(),
            fence.database_identity(),
            fence.lifecycle_generation(),
            fence.source_revision(),
            lease_epoch + 1,
        )
        .expect("stale generation observation");
        let stale_generation_spec = LiveChunkEffectSpec::new(
            effect_id,
            output,
            target,
            vec![stale_generation],
            0,
            3,
            b"abc",
        )
        .expect("stale generation effect");
        let mut stale_generation_issue = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("stale generation pin"),
                stale_generation_spec,
            )
            .expect("stale generation issue queues");
        assert!(
            stale_generation_issue
                .recv_timeout(Duration::from_secs(2))
                .expect("stale generation issue result")
                .is_err()
        );
        let stale_lifecycle = SourceFence::new(
            fence.source_id().clone(),
            fence.root_identity(),
            fence.database_identity(),
            fence.lifecycle_generation() + 1,
            fence.source_revision(),
            lease_epoch,
        )
        .expect("stale lifecycle observation");
        let stale_lifecycle_spec = LiveChunkEffectSpec::new(
            effect_id,
            output,
            target,
            vec![stale_lifecycle],
            0,
            3,
            b"abc",
        )
        .expect("stale lifecycle effect");
        let mut stale_lifecycle_issue = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("stale lifecycle pin"),
                stale_lifecycle_spec,
            )
            .expect("stale lifecycle issue queues");
        assert!(
            stale_lifecycle_issue
                .recv_timeout(Duration::from_secs(2))
                .expect("stale lifecycle issue result")
                .is_err()
        );
        let foreign_participant_fence = SourceFence::new(
            SourceId::from_string(String::from("foreign-source")),
            fence.root_identity(),
            fence.database_identity(),
            fence.lifecycle_generation(),
            fence.source_revision(),
            lease_epoch,
        )
        .expect("foreign participant observation");
        let foreign_spec = LiveChunkEffectSpec::new(
            effect_id,
            output,
            target,
            vec![foreign_participant_fence],
            0,
            3,
            b"abc",
        )
        .expect("bounded foreign observation");
        let mut foreign = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard
                    .pin_for_effect()
                    .expect("source pin for foreign denial"),
                foreign_spec,
            )
            .expect("foreign issue queues");
        assert!(
            format!(
                "{}",
                foreign
                    .recv_timeout(Duration::from_secs(2))
                    .expect("foreign issue result")
                    .expect_err("foreign participant cannot issue")
            )
            .contains("absent from the operation")
        );
        let spec =
            LiveChunkEffectSpec::new(effect_id, output, target, vec![fence.clone()], 0, 3, b"abc")
                .expect("bounded effect");
        let mut issue = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("source pin"),
                spec,
            )
            .expect("live issue queues");
        let grant = issue
            .recv_timeout(Duration::from_secs(2))
            .expect("live issue result")
            .expect("live issue worker");
        assert_eq!(grant.permit.chunk_id(), claim.chunk_id());
        assert_eq!(grant.permit.operation_id(), operation_id);
        assert_eq!(grant.output_key, "opaque-output-key");
        assert_eq!(grant.spec.effect_id, effect_id);
        assert_eq!(grant.spec.output, output);
        assert_eq!(grant.spec.target, target);
        assert_eq!(grant.spec.payload_bytes, 3);
        assert_eq!(grant.spec.payload_hash, *blake3::hash(b"abc").as_bytes());
        let changed_payload = file_owner.live_chunk_effect_for_test(&grant, b"abd".to_vec());
        assert!(FileOperationCommand::for_live_chunk(&grant, changed_payload).is_err());
        let effect = file_owner.live_chunk_effect_for_test(&grant, b"abc".to_vec());
        let command =
            FileOperationCommand::for_live_chunk(&grant, effect).expect("grant-bound file command");
        let retry_command = command
            .retry_for_live_chunk()
            .expect("authority-free retry view");
        let stopped_owner = FileOperationOwner::start().expect("stopped owner fixture");
        let _ = stopped_owner.shutdown();
        let (_rejection, grant) = match stopped_owner.submit_live_chunk(command, grant) {
            Err(rejected) => rejected,
            Ok(_) => panic!("stopped owner accepted a grant"),
        };
        let dropped_reply = file_owner
            .submit_live_chunk(
                retry_command
                    .retry_for_live_chunk()
                    .expect("same bounded command"),
                grant,
            )
            .expect("file-owner submission");
        drop(dropped_reply);
        let mut retry = file_owner
            .retry_live_chunk(retry_command)
            .expect("exact retry admission");
        let result = retry
            .recv_timeout(Duration::from_secs(2))
            .expect("retained effect result");
        assert!(matches!(
            result.outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        assert_eq!(file_owner.execution_count_for_test(), 1);
        let unused_grant = retry
            .take_unused_live_chunk_grant()
            .expect("recover confirmed unused grant")
            .expect("one sealed grant retained");
        assert!(
            retry
                .take_unused_live_chunk_grant()
                .expect("second recovery request")
                .is_none()
        );
        retry.acknowledge().expect("acknowledge retained result");

        let replay_spec =
            LiveChunkEffectSpec::new(effect_id, output, target, vec![fence.clone()], 0, 3, b"abc")
                .expect("replay observations");
        let mut replay = client
            .issue_live_chunk_effect(
                claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("second source pin"),
                replay_spec,
            )
            .expect("replay queues");
        assert!(
            format!(
                "{}",
                replay
                    .recv_timeout(Duration::from_secs(2))
                    .expect("replay result")
                    .expect_err("dropped grant cannot reissue")
            )
            .contains("already Issued")
        );
        let mut release = client
            .release_chunk(unused_grant.into_unused_permit(), Uuid::new_v4())
            .expect("confirmed no-effect release queues");
        release
            .recv_timeout(Duration::from_secs(2))
            .expect("release result")
            .expect("journal release worker");

        let stale_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                claim.claim().volume().clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("stale claim"),
            participant_id,
            String::from("stale-root-output"),
            facts.allocation_unit,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("stale root chunk request");
        let mut stale_claim_result = client
            .claim_chunk(stale_claim.clone())
            .expect("stale root claim queues");
        stale_claim_result
            .recv_timeout(Duration::from_secs(2))
            .expect("stale root claim result")
            .expect("stale root claim worker");
        let stale_spec = LiveChunkEffectSpec::new(
            EffectId::new(),
            OutputIdentity::new(),
            target,
            vec![fence.clone()],
            facts.allocation_unit,
            3,
            b"xyz",
        )
        .expect("stale root effect");
        let mut stale_issue = client
            .issue_live_chunk_effect(
                stale_claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("stale root source pin"),
                stale_spec,
            )
            .expect("stale root issue queues");
        let stale_grant = stale_issue
            .recv_timeout(Duration::from_secs(2))
            .expect("stale root issue result")
            .expect("stale root issue worker");
        let stale_effect = file_owner.live_chunk_effect_for_test(&stale_grant, b"xyz".to_vec());
        let stale_command = FileOperationCommand::for_live_chunk(&stale_grant, stale_effect)
            .expect("stale root command");
        file_owner
            .retire_root_for_test(target)
            .expect("retire root before effect");
        let mut stale_request = file_owner
            .submit_live_chunk(stale_command, stale_grant)
            .expect("stale root file request queues");
        let stale_result = stale_request
            .recv_timeout(Duration::from_secs(2))
            .expect("stale root result");
        assert!(matches!(
            stale_result.outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
        ));
        assert_eq!(file_owner.execution_count_for_test(), 1);
        let stale_unused = stale_request
            .take_unused_live_chunk_grant()
            .expect("stale grant recovery")
            .expect("stale grant retained");
        stale_request.acknowledge().expect("stale result ack");
        let mut stale_release = client
            .release_chunk(stale_unused.into_unused_permit(), Uuid::new_v4())
            .expect("stale release queues");
        stale_release
            .recv_timeout(Duration::from_secs(2))
            .expect("stale release result")
            .expect("stale release worker");
        let live_target = file_owner
            .register_live_chunk_root_for_test()
            .expect("fresh staging root");
        let source_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                claim.claim().volume().clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("source claim"),
            participant_id,
            String::from("retired-source-output"),
            facts.allocation_unit * 2,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("source chunk request");
        let mut source_claim_result = client
            .claim_chunk(source_claim.clone())
            .expect("source claim queues");
        source_claim_result
            .recv_timeout(Duration::from_secs(2))
            .expect("source claim result")
            .expect("source claim worker");
        let source_spec = LiveChunkEffectSpec::new(
            EffectId::new(),
            OutputIdentity::new(),
            live_target,
            vec![fence.clone()],
            facts.allocation_unit * 2,
            3,
            b"end",
        )
        .expect("source retirement effect");
        let mut source_issue = client
            .issue_live_chunk_effect(
                source_claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("source retirement pin"),
                source_spec,
            )
            .expect("source retirement issue queues");
        let source_grant = source_issue
            .recv_timeout(Duration::from_secs(2))
            .expect("source retirement issue result")
            .expect("source retirement issue worker");
        let source_effect = file_owner.live_chunk_effect_for_test(&source_grant, b"end".to_vec());
        let source_command = FileOperationCommand::for_live_chunk(&source_grant, source_effect)
            .expect("source retirement command");
        let uncertain_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                claim.claim().volume().clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("uncertain claim"),
            participant_id,
            String::from("uncertain-output"),
            facts.allocation_unit * 3,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("uncertain chunk request");
        let mut uncertain_claim_result = client
            .claim_chunk(uncertain_claim.clone())
            .expect("uncertain claim queues");
        uncertain_claim_result
            .recv_timeout(Duration::from_secs(2))
            .expect("uncertain claim result")
            .expect("uncertain claim worker");
        let uncertain_spec = LiveChunkEffectSpec::new(
            EffectId::new(),
            OutputIdentity::new(),
            live_target,
            vec![fence.clone()],
            facts.allocation_unit * 3,
            3,
            b"odd",
        )
        .expect("uncertain effect");
        let mut uncertain_issue = client
            .issue_live_chunk_effect(
                uncertain_claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("uncertain source pin"),
                uncertain_spec,
            )
            .expect("uncertain issue queues");
        let uncertain_grant = uncertain_issue
            .recv_timeout(Duration::from_secs(2))
            .expect("uncertain issue result")
            .expect("uncertain issue worker");
        let uncertain_effect =
            file_owner.live_chunk_effect_for_test(&uncertain_grant, b"odd".to_vec());
        let uncertain_command =
            FileOperationCommand::for_live_chunk(&uncertain_grant, uncertain_effect)
                .expect("uncertain command");
        file_owner.force_next_live_chunk_uncertain_for_test();
        let mut uncertain_request = file_owner
            .submit_live_chunk(uncertain_command, uncertain_grant)
            .expect("uncertain file request queues");
        assert!(matches!(
            uncertain_request
                .recv_timeout(Duration::from_secs(2))
                .expect("uncertain result")
                .outcome(),
            EffectOutcome::RetainedUncertain { .. }
        ));
        assert!(
            uncertain_request
                .take_unused_live_chunk_grant()
                .expect("uncertain recovery query")
                .is_none()
        );
        uncertain_request
            .acknowledge()
            .expect("uncertain result ack");
        let mut uncertain_reissue = client
            .issue_chunk(uncertain_claim.chunk_id(), operation_id)
            .expect("uncertain reissue queues");
        assert!(
            uncertain_reissue
                .recv_timeout(Duration::from_secs(2))
                .expect("uncertain reissue result")
                .is_err()
        );
        let staged_file = source_root.path().join("staging-owner-check.bin");
        fs::write(&staged_file, b"abc").expect("staged source content");
        let staged_object = crate::file_operation_owner::OwnedObjectIdentity::new();
        let staged_output = OutputIdentity::new();
        let staging_participant_id = operation
            .universal_intent
            .as_ref()
            .expect("staging operation intent")
            .participants()
            .descriptors()
            .iter()
            .find(|participant| participant.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let staged_command = || {
            FileOperationCommand::for_staging_validation(
                operation_id,
                staging_participant_id,
                EffectId::new(),
                staged_output,
                source_root_registration,
                crate::file_operation_owner::AuthorityFence::Source(fence.clone()),
                crate::file_operation_owner::BoundedRelativeLocator::new("staging-owner-check.bin")
                    .expect("staging locator"),
                staged_object,
                crate::file_operation_owner::StagingValidation::new(
                    3,
                    *blake3::hash(b"abc").as_bytes(),
                ),
                vec![fence.clone()],
                Some(source_root_registration),
            )
            .expect("source-bound staging command")
        };
        let mut staged = file_owner
            .submit(staged_command())
            .expect("source-bound validation queues");
        let original_staged_result = staged
            .recv_original_staging_validation(Duration::from_secs(2))
            .expect("source-bound validation result");
        let staged_result = (*original_staged_result).clone();
        let EffectOutcome::Applied { evidence } = staged_result.outcome() else {
            panic!("expected source-bound staging validation");
        };
        let proof = evidence.validated_staging().expect("staging proof");
        assert_eq!(proof.output(), staged_output);
        assert_eq!(proof.source_root(), Some(source_root_registration));
        let checkpoint_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                claim.claim().volume().clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("checkpoint claim"),
            staging_participant_id,
            String::from("staging-owner-check.bin"),
            facts.allocation_unit * 4,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("checkpoint chunk request");
        let mut checkpoint_claim_result = client
            .claim_chunk(checkpoint_claim.clone())
            .expect("checkpoint claim queues");
        checkpoint_claim_result
            .recv_timeout(Duration::from_secs(2))
            .expect("checkpoint claim result")
            .expect("checkpoint claim worker");
        let checkpoint_spec = LiveChunkEffectSpec::new(
            EffectId::new(),
            staged_output,
            source_root_registration,
            vec![fence.clone()],
            facts.allocation_unit * 4,
            3,
            b"abc",
        )
        .expect("checkpoint live effect spec");
        let mut checkpoint_issue = client
            .issue_live_chunk_effect(
                checkpoint_claim.chunk_id(),
                operation_id,
                registration.clone(),
                guard.pin_for_effect().expect("checkpoint issue pin"),
                checkpoint_spec,
            )
            .expect("checkpoint issue queues");
        let checkpoint_grant = checkpoint_issue
            .recv_timeout(Duration::from_secs(2))
            .expect("checkpoint issue result")
            .expect("checkpoint issue worker");
        let publication_charge_selector = checkpoint_grant.publication_charge_selector();
        let original_publication_issue = checkpoint_grant.issue_id();
        let mut wrong_chunk_checkpoint = client
            .checkpoint_live_chunk_staging(
                checkpoint_claim.chunk_id(),
                Uuid::new_v4(),
                staged_result.clone(),
                registration.clone(),
                guard.pin_for_effect().expect("wrong checkpoint pin"),
            )
            .expect("wrong checkpoint queues");
        assert!(
            wrong_chunk_checkpoint
                .recv_timeout(Duration::from_secs(2))
                .expect("wrong checkpoint result")
                .is_err()
        );
        let mut chunk_checkpoint = client
            .checkpoint_live_chunk_staging(
                checkpoint_claim.chunk_id(),
                checkpoint_grant.issue_id(),
                staged_result.clone(),
                registration.clone(),
                guard.pin_for_effect().expect("chunk checkpoint pin"),
            )
            .expect("chunk checkpoint queues");
        assert!(matches!(
            chunk_checkpoint
                .recv_timeout(Duration::from_secs(2))
                .expect("chunk checkpoint result")
                .expect("chunk checkpoint worker"),
            ChunkStagingReceipt::Committed { .. }
        ));
        let mut denied_checkpoint_release = client
            .release_chunk(checkpoint_grant.into_unused_permit(), Uuid::new_v4())
            .expect("checkpoint release queues");
        assert!(
            denied_checkpoint_release
                .recv_timeout(Duration::from_secs(2))
                .expect("checkpoint release result")
                .is_err()
        );
        let publication_root = file_owner
            .register_profile_root(&fixture.owner)
            .expect("original profile publication root");
        let prepare_target = || {
            let mut pending = file_owner
                .prepare_publication_target(
                    Arc::clone(&original_staged_result),
                    publication_root,
                    crate::file_operation_owner::BoundedRelativeLocator::new(
                        "published-owner-check.bin",
                    )
                    .unwrap(),
                    EffectId::new(),
                    Instant::now() + Duration::from_secs(2),
                )
                .expect("original publication target queues");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(target) = pending
                    .try_target()
                    .expect("original publication target result")
                {
                    break target;
                }
                assert!(
                    Instant::now() < deadline,
                    "original publication target timed out"
                );
                std::thread::yield_now();
            }
        };
        let input_before = owner
            .shared
            .input_bytes
            .lock()
            .unwrap()
            .publication_remaining;
        if let Some(kind) = stale_target {
            let target = prepare_target();
            match kind {
                0 => file_owner.retire_root(publication_root).unwrap(),
                1 => {
                    staged.acknowledge().unwrap();
                    let _ = file_owner.shutdown();
                }
                2 => std::thread::sleep(Duration::from_millis(2050)),
                _ => unreachable!(),
            }
            assert!(!target.is_current());
            let mut refused = client
                .checkpoint_and_grant_staging(
                    target,
                    publication_charge_selector,
                    registration.clone(),
                    guard.pin_for_effect().unwrap(),
                )
                .unwrap();
            assert!(matches!(
                refused.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(OperationJournalOwnerError::StagingGrant { .. })
            ));
            assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
            assert_eq!(
                owner
                    .shared
                    .input_bytes
                    .lock()
                    .unwrap()
                    .publication_remaining,
                input_before
            );
            drop(guard.pin_for_effect().unwrap().begin_effect().unwrap());
            if kind != 1 {
                staged.acknowledge().unwrap();
                let _ = file_owner.shutdown();
            }
            drop(database);
            let _ = owner.shutdown();
            let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
            assert!(
                reopened
                    .checkpoint_staging_result(&staged_result)
                    .unwrap()
                    .is_some(),
                "stale target refused before any staging checkpoint"
            );
            return;
        }
        let unused_for_release = release_after_entry.map(|_| {
            let request = ChunkClaimRequest::new(ChunkId::new(), operation_id,
                AdmissionClaim::new(AdmissionClaimId::new(), claim.claim().volume().clone(),
                    crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                    facts.allocation_unit).unwrap(), participant_id, "fenced-unused-release".into(),
                0, 1, facts.allocation_unit, PROTECTED_FREE_SPACE_FLOOR).unwrap();
            let mut claimed = client.claim_chunk(request.clone()).unwrap();
            claimed.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            let spec = LiveChunkEffectSpec::new(EffectId::new(), OutputIdentity::new(),
                publication_root, vec![fence.clone()], 0, 1, b"x").unwrap();
            let mut issued = client.issue_live_chunk_effect(request.chunk_id(), operation_id,
                registration.clone(), guard.pin_for_effect().unwrap(), spec).unwrap();
            let unused = issued.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            (request, unused.into_unused_permit())
        });
        if let Some((nth, _)) = fault {
            armed.store(nth, Ordering::Release);
        }
        let mut checkpoint = client
            .checkpoint_and_grant_staging(
                prepare_target(),
                publication_charge_selector,
                registration.clone(),
                guard.pin_for_effect().expect("checkpoint source pin"),
            )
            .expect("staging checkpoint queues");
        let publication_result = checkpoint
            .recv_timeout(Duration::from_secs(2))
            .expect("staging checkpoint result");
        if fault.is_some() {
            assert!(matches!(
                publication_result,
                Err(OperationJournalOwnerError::Store(
                    OperationJournalStoreError::StagingOutcomeUncertain { .. }
                ))
            ));
            assert_eq!(armed.load(Ordering::Acquire), 0);
            assert!(matches!(
                owner.shared.resource_admission.validate_open(),
                Err(resource_custody::UseError::Unknown)
            ));
            let mut refused = client
                .checkpoint_and_grant_staging(
                    prepare_target(),
                    publication_charge_selector,
                    registration.clone(),
                    guard.pin_for_effect().unwrap(),
                )
                .unwrap();
            assert!(matches!(
                refused.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(OperationJournalOwnerError::StagingGrant { .. })
            ));
            staged.acknowledge().unwrap();
            let _ = file_owner.shutdown();
            drop(database);
            assert!(!owner.shutdown());
            return;
        }
        let publication = publication_result.expect("acknowledged publication grant");
        assert_eq!(
            owner
                .shared
                .input_bytes
                .lock()
                .unwrap()
                .publication_remaining,
            input_before
                - 64 * 1024
                - publication_fence::HEADER_BYTES
                - native_read_capsule::BACKING
                - native_read_capsule::REGISTRY
        );
        if let Some(entered) = release_after_entry {
            let (request, unused) = unused_for_release.unwrap();
            let release = Uuid::new_v4();
            if entered {
                let permit = publication.begin_publication().unwrap();
                let mut returned = client.release_chunk(unused, release).unwrap();
                // Queue admission is nonblocking; wait for actor replies only after source Drop.
                drop(permit);
                assert!(matches!(
                    returned.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Err(OperationJournalOwnerError::PublicationChargeReleasePending { .. })
                ));
                let mut retry = client
                    .retry_chunk_release(request.chunk_id(), operation_id, release)
                    .unwrap();
                assert!(matches!(
                    retry.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Err(OperationJournalOwnerError::PublicationChargeReleasePending { .. })
                ));
                let mut claimed = client.claim_chunk(request.clone()).unwrap();
                assert_eq!(
                    claimed
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap()
                        .unwrap()
                        .state(),
                    ChunkClaimState::Issued
                );
                assert_eq!(owner.shared.resource_admission.snapshot(), (false, 0, true));
            } else {
                let mut returned = client.release_chunk(unused, release).unwrap();
                returned
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert!(publication.begin_publication().is_err());
                assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
                drop(guard.pin_for_effect().unwrap().begin_effect().unwrap());
            }
            staged.acknowledge().unwrap();
            let _ = file_owner.shutdown();
            drop(database);
            let _ = owner.shutdown();
            return;
        }
        if let Some(entered) = cancel_after_entry {
            let mut scan = client.scan_recovery_page(None).unwrap();
            let page = scan.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            let sequence = page
                .entries()
                .iter()
                .find(|entry| {
                    matches!(entry.key(),
                super::super::store::recovery::RecoveryKey::OperationId(id) if *id == operation_id)
                })
                .unwrap()
                .sequence();
            let before = encode(&operation).unwrap();
            let observation =
                CancellationRequestObservation::new(operation.updated_unix_ms + 1).unwrap();
            if entered {
                let permit = publication.begin_publication().unwrap();
                assert_eq!(owner.shared.resource_admission.snapshot().1, 1);
                // Queue admission is nonblocking; never wait for a reply while holding source custody.
                let mut cancel = client
                    .request_cancel_before_publish(operation_id, sequence, observation)
                    .unwrap();
                drop(permit);
                assert!(matches!(
                    cancel.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Err(OperationJournalOwnerError::PublicationCancellationPending { .. })
                ));
                let mut lookup = client.get(operation_id).unwrap();
                assert_eq!(
                    encode(
                        &lookup
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .unwrap()
                            .unwrap()
                    )
                    .unwrap(),
                    before
                );
                assert_eq!(owner.shared.resource_admission.snapshot(), (false, 0, true));
            } else {
                let mut cancel = client
                    .request_cancel_before_publish(operation_id, sequence, observation)
                    .unwrap();
                cancel
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert!(publication.begin_publication().is_err());
                assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
                drop(guard.pin_for_effect().unwrap().begin_effect().unwrap());
            }
            staged.acknowledge().unwrap();
            let _ = file_owner.shutdown();
            drop(database);
            let _ = owner.shutdown();
            return;
        }
        if let Some(unavailable) = close_before_entry {
            assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
            if unavailable {
                owner
                    .shared
                    .mark_unavailable("native test: original unavailable after issuance");
            } else {
                assert!(owner.begin_command_drain());
            }
            assert!(matches!(
                publication.begin_publication(),
                Err(OperationJournalOwnerError::StagingGrant { .. })
            ));
            assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
            // This original source remains independently live: refusal came from the
            // original journal gate, not a fake pin or a drained source fixture.
            drop(guard.pin_for_effect().unwrap().begin_effect().unwrap());
            staged.acknowledge().unwrap();
            let _ = file_owner.shutdown();
            drop(database);
            let clean = owner.shutdown();
            if unavailable {
                assert!(!clean);
            }
            return;
        }
        if publication_handoff {
            let original_command_bytes = publication
                .original_target()
                .publication_command()
                .unwrap()
                .canonical_bytes()
                .unwrap();
            let original_command: serde_json::Value =
                serde_json::from_slice(&original_command_bytes).unwrap();
            let original_charge = serde_json::to_value(publication.charge.claim()).unwrap();
            let original_command_fingerprint = *blake3::hash(&original_command_bytes).as_bytes();
            let original_slot_generation = publication.fence.generation_observation_for_test();
            drop(original_command_bytes);
            let original_fingerprint = publication.original_target().fingerprint();
            let foreign = crate::file_operation_owner::FileOperationOwner::start().unwrap();
            let publication = match foreign.submit_publication(publication) {
                Err((
                    crate::file_operation_owner::FileOperationOwnerQueueError::InvalidInput(_),
                    original,
                )) => original,
                _ => panic!("foreign owner must return the exact original grant before admission"),
            };
            assert_eq!(
                publication.original_target().fingerprint(),
                original_fingerprint
            );
            assert_eq!(foreign.execution_count_for_test(), 0);
            assert_eq!(
                foreign.shutdown(),
                crate::file_operation_owner::FileOperationOwnerShutdown::DrainedAck
            );
            let retry = publication.original_target().publication_command().unwrap();
            let exact = publication.original_target().publication_command().unwrap();
            let completion = publication.fence.completion_observer();
            let pending = file_owner.submit_publication(publication).unwrap();
            drop(pending); // Accepted loss retains the original grant; no entry happened.
            let mut duplicate = file_owner.submit(retry).unwrap();
            let result = duplicate.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                result.outcome(),
                crate::file_operation_owner::EffectOutcome::Unsupported { .. }
            ));
            // An actual queued read is an actor barrier. Its dispatch scans the original
            // completion first and confirms the original no-effect record. Charges remain.
            let mut readback = client.get(operation_id).unwrap();
            assert!(
                readback
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .is_ok()
            );
            assert!(completion());
            assert!(duplicate.take_unused_live_chunk_grant().unwrap().is_none());
            duplicate.acknowledge().unwrap();
            let mut exact_again = file_owner.submit(exact).unwrap();
            assert_eq!(
                exact_again.recv_timeout(Duration::from_secs(2)).unwrap(),
                result
            );
            exact_again.acknowledge().unwrap();
            assert_eq!(owner.shared.resource_admission.snapshot().1, 0);
            assert_eq!(
                owner
                    .shared
                    .input_bytes
                    .lock()
                    .unwrap()
                    .publication_remaining,
                input_before
                    - 64 * 1024
                    - publication_fence::HEADER_BYTES
                    - native_read_capsule::BACKING
                    - native_read_capsule::REGISTRY
            );
            staged.acknowledge().unwrap();
            let _ = file_owner.shutdown();
            drop(database);
            let original_shared = Arc::clone(&owner.shared);
            let publication_baseline = InputBudget::new().publication_remaining;
            let _ = owner.shutdown();
            // This genuine observer retains only the original fence allocation after
            // both actual workers joined; neither a grant nor runtime resource survives.
            assert!(completion());
            assert_eq!(
                original_shared
                    .input_bytes
                    .lock()
                    .unwrap()
                    .publication_remaining,
                publication_baseline - publication_fence::HEADER_BYTES
            );
            drop(completion);
            assert_eq!(
                original_shared
                    .input_bytes
                    .lock()
                    .unwrap()
                    .publication_remaining,
                publication_baseline
            );
            let restarted =
                OperationJournalStore::open(&fixture.owner).expect("original handoff restart");
            let binding =
                restarted.publication_binding_observation_for_test(staged_output.as_uuid());
            assert_eq!(binding["schema_version"], serde_json::json!(2));
            assert_eq!(binding["no_effect"]["kind"], serde_json::json!(1));
            assert_eq!(binding["no_effect"]["digest"].as_array().unwrap().len(), 32);
            assert_eq!(binding["command"], original_command);
            assert_eq!(binding["charge"], original_charge);
            assert_eq!(
                binding["command_fingerprint"],
                serde_json::json!(original_command_fingerprint)
            );
            assert_eq!(
                binding["chunk"]["chunk_id"],
                serde_json::json!(checkpoint_claim.chunk_id().as_uuid())
            );
            assert_eq!(
                binding["chunk"]["issue_id"],
                serde_json::json!(original_publication_issue)
            );
            assert_eq!(
                binding["slot_generation"],
                serde_json::json!(original_slot_generation)
            );
            assert!(
                !Uuid::parse_str(binding["issuer_incarnation"].as_str().unwrap())
                    .unwrap()
                    .is_nil()
            );
            assert!(binding["checkpoint_sequence"].as_i64().unwrap() > 0);
            assert!(restarted.prove_clean_handoff_durable().is_err());
            return;
        }
        let permit = publication
            .begin_publication()
            .expect("live publication entry");
        assert_eq!(permit.validation().operation_id(), operation_id);
        assert_eq!(owner.shared.resource_admission.snapshot().1, 1);
        assert_eq!(
            owner
                .shared
                .input_bytes
                .lock()
                .unwrap()
                .publication_remaining,
            input_before
                - 64 * 1024
                - publication_fence::HEADER_BYTES
                - native_read_capsule::BACKING
                - native_read_capsule::REGISTRY
        );
        drop(permit);
        assert_eq!(
            owner
                .shared
                .input_bytes
                .lock()
                .unwrap()
                .publication_remaining,
            input_before - publication_fence::HEADER_BYTES - native_read_capsule::REGISTRY
        );
        assert_eq!(owner.shared.resource_admission.snapshot(), (false, 0, true));
        let mut replay_checkpoint = client
            .checkpoint_and_grant_staging(
                prepare_target(),
                publication_charge_selector,
                registration.clone(),
                guard.pin_for_effect().expect("replay source pin"),
            )
            .expect("replay checkpoint queues");
        assert!(
            replay_checkpoint
                .recv_timeout(Duration::from_secs(2))
                .expect("replay checkpoint result")
                .is_err()
        );
        staged.acknowledge().expect("staging result ack");
        drop(guard);
        let mut stale_staged = file_owner
            .submit(staged_command())
            .expect("stale staging validation queues");
        assert!(matches!(
            stale_staged
                .recv_timeout(Duration::from_secs(2))
                .expect("stale staging validation result")
                .outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
        ));
        stale_staged
            .acknowledge()
            .expect("stale staging result ack");
        assert!(
            file_owner
                .register_source_root(Arc::clone(&source_request), &fixture.owner, fence.clone())
                .is_err()
        );
        file_owner
            .retire_root(source_root_registration)
            .expect("retire stale source root");
        let mut source_result = file_owner
            .submit_live_chunk(source_command, source_grant)
            .expect("source retirement file request queues");
        assert!(matches!(
            source_result
                .recv_timeout(Duration::from_secs(2))
                .expect("source retirement result")
                .outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
        ));
        assert_eq!(file_owner.execution_count_for_test(), 2);
        let unused_source = source_result
            .take_unused_live_chunk_grant()
            .expect("source grant recovery")
            .expect("source grant retained");
        source_result.acknowledge().expect("source result ack");
        let mut source_release = client
            .release_chunk(unused_source.into_unused_permit(), Uuid::new_v4())
            .expect("source release queues");
        source_release
            .recv_timeout(Duration::from_secs(2))
            .expect("source release result")
            .expect("source release worker");
        assert_eq!(owner.synthetic_chunk_write_invocations(), 0);
        assert!(matches!(
            file_owner.shutdown(),
            crate::file_operation_owner::FileOperationOwnerShutdown::RetirementDeferred { .. }
        ));
        drop(database);
        let _ = owner.shutdown();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn post_intent_chunk_claim_issue_release_and_replay_are_owner_serialized() {
        let fixture = fixture("journal-owner-chunk-claim-lifecycle");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            registry_path.clone(),
        )
        .expect("journal owner");
        wait_until_available(&owner);

        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        );
        let operation_id = {
            let mut request = owner
                .client()
                .admit_prepared(input)
                .expect("initial operation admission queues");
            request
                .recv_timeout(Duration::from_secs(2))
                .expect("initial operation result")
                .expect("initial operation worker")
        };
        let participant_id = crate::operation_journal::ParticipantId::new();
        let client = owner.client();
        let first = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
                AdmissionClaim::new(
                    AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("first chunk claim"),
            participant_id,
            String::from("chunk-a"),
            0,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("first chunk request");

        // The prepared copy fixture has its real participant identity in its envelope. Read it
        // from the persisted operation before queueing the post-intent row.
        let mut operation_lookup = client.get(operation_id).expect("operation lookup queues");
        let operation = operation_lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("operation lookup result")
            .expect("operation lookup worker")
            .expect("operation record");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let first = ChunkClaimRequest::new(
            first.chunk_id(),
            operation_id,
            first.claim().clone(),
            participant_id,
            first.output_key().to_owned(),
            first.logical_offset(),
            first.logical_length(),
            first.allocation_unit(),
            first.protected_free_bytes(),
        )
        .expect("participant-bound first chunk request");

        let mut claim_request = client
            .claim_chunk(first.clone())
            .expect("chunk claim queues");
        let claimed = claim_request
            .recv_timeout(Duration::from_secs(2))
            .expect("chunk claim result")
            .expect("chunk claim worker");
        assert_eq!(claimed.state(), ChunkClaimState::Claimed);
        assert!(claimed.matches_request(&first));

        let mut duplicate = client
            .claim_chunk(first.clone())
            .expect("duplicate chunk claim queues");
        assert_eq!(
            duplicate
                .recv_timeout(Duration::from_secs(2))
                .expect("duplicate result")
                .expect("duplicate worker"),
            claimed
        );

        let mut issue = client
            .issue_chunk(first.chunk_id(), operation_id)
            .expect("issue queues");
        let permit = issue
            .recv_timeout(Duration::from_secs(2))
            .expect("issue result")
            .expect("issue worker");
        let issue_id = permit.issue_id();
        let mut issue_replay = client
            .issue_chunk(first.chunk_id(), operation_id)
            .expect("issued replay queues");
        let issue_replay = issue_replay
            .recv_timeout(Duration::from_secs(2))
            .expect("issued replay result")
            .expect_err("issued chunk must never mint a second permit");
        assert!(format!("{issue_replay}").contains("already Issued"));
        let release_id = Uuid::new_v4();
        let mut release = client
            .release_chunk(permit, release_id)
            .expect("release queues");
        let released = release
            .recv_timeout(Duration::from_secs(2))
            .expect("release result")
            .expect("release worker");
        assert_eq!(released.release_id(), release_id);
        assert_eq!(released.operation_id(), operation_id);

        let mut retry = client
            .retry_chunk_release(first.chunk_id(), operation_id, release_id)
            .expect("release replay queues");
        assert_eq!(
            retry
                .recv_timeout(Duration::from_secs(2))
                .expect("release replay result")
                .expect("release replay worker"),
            released
        );

        // A released chunk remains in durable history while the initial operation claim stays
        // live. A later independent chunk must reconcile that grouped history without treating
        // the released token as an extra live charge or resurrecting it.
        let second = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("second chunk claim"),
            first.participant_id(),
            String::from("chunk-b"),
            facts.allocation_unit,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("second chunk request");
        let mut second_claim = client
            .claim_chunk(second.clone())
            .expect("second chunk claim queues");
        assert_eq!(
            second_claim
                .recv_timeout(Duration::from_secs(2))
                .expect("second claim result")
                .expect("second claim worker")
                .state(),
            ChunkClaimState::Claimed
        );
        let mut second_cancel = client
            .cancel_chunk(second.chunk_id(), operation_id, Uuid::new_v4())
            .expect("second cancellation queues");
        second_cancel
            .recv_timeout(Duration::from_secs(2))
            .expect("second cancellation result")
            .expect("second cancellation worker");

        // An owner-held Issued ticket may be rejected at the synthetic entry boundary without
        // becoming forgeable from its durable row. The consumed queue ticket is not converted
        // into release authority by readback IDs, while id-only cancellation cannot release
        // Issued state.
        let guarded = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("guarded chunk claim"),
            first.participant_id(),
            String::from("chunk-guarded"),
            facts.allocation_unit * 4,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("guarded chunk request");
        let mut guarded_claim = client
            .claim_chunk(guarded.clone())
            .expect("guarded claim queues");
        guarded_claim
            .recv_timeout(Duration::from_secs(2))
            .expect("guarded claim result")
            .expect("guarded claim worker");
        let reserve_len = fs::metadata(&reserve_path).expect("reserve metadata").len();
        fs::OpenOptions::new()
            .write(true)
            .open(&reserve_path)
            .expect("open reserve for low-space issue")
            .set_len(PROTECTED_FREE_SPACE_FLOOR - 1)
            .expect("shorten reserve before issue");
        let mut low_issue = client
            .issue_chunk(guarded.chunk_id(), operation_id)
            .expect("low-space issue queues");
        assert!(
            low_issue
                .recv_timeout(Duration::from_secs(2))
                .expect("low-space issue result")
                .is_err()
        );
        fs::OpenOptions::new()
            .write(true)
            .open(&reserve_path)
            .expect("reopen reserve after low-space issue")
            .set_len(reserve_len)
            .expect("restore reserve after low-space issue");
        let mut guarded_issue = client
            .issue_chunk(guarded.chunk_id(), operation_id)
            .expect("guarded issue queues");
        let guarded_permit = guarded_issue
            .recv_timeout(Duration::from_secs(2))
            .expect("guarded issue result")
            .expect("guarded issue worker");
        let mut issued_cancel = client
            .cancel_chunk(guarded.chunk_id(), operation_id, Uuid::new_v4())
            .expect("issued cancellation queues");
        assert!(
            issued_cancel
                .recv_timeout(Duration::from_secs(2))
                .expect("issued cancellation result")
                .is_err()
        );
        fs::OpenOptions::new()
            .write(true)
            .open(&reserve_path)
            .expect("open reserve for low-space entry")
            .set_len(PROTECTED_FREE_SPACE_FLOOR - 1)
            .expect("shorten reserve before entry");
        let before_low_space_entry = owner.synthetic_chunk_write_invocations();
        let mut denied_write = client
            .begin_chunk_write(guarded_permit, guarded.identity(), guarded.logical_length())
            .expect("low-space entry queues");
        assert!(
            denied_write
                .recv_timeout(Duration::from_secs(2))
                .expect("low-space entry result")
                .is_err()
        );
        assert_eq!(
            owner.synthetic_chunk_write_invocations(),
            before_low_space_entry
        );
        fs::OpenOptions::new()
            .write(true)
            .open(&reserve_path)
            .expect("reopen reserve after low-space entry")
            .set_len(reserve_len)
            .expect("restore reserve after low-space entry");
        let mut guarded_release = client
            .retry_chunk_release(guarded.chunk_id(), operation_id, Uuid::new_v4())
            .expect("readback-only release retry queues");
        assert!(
            guarded_release
                .recv_timeout(Duration::from_secs(2))
                .expect("readback-only release retry result")
                .is_err()
        );

        let restart_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("restart chunk claim"),
            first.participant_id(),
            String::from("chunk-restart"),
            facts.allocation_unit * 2,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("restart chunk request");
        let issued_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("issued chunk claim"),
            first.participant_id(),
            String::from("chunk-issued"),
            facts.allocation_unit * 3,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("issued chunk request");
        let mut restart_claim_request = client
            .claim_chunk(restart_claim.clone())
            .expect("restart chunk claim queues");
        restart_claim_request
            .recv_timeout(Duration::from_secs(2))
            .expect("restart chunk claim result")
            .expect("restart chunk claim worker");
        let mut issued_claim_request = client
            .claim_chunk(issued_claim.clone())
            .expect("issued chunk claim queues");
        issued_claim_request
            .recv_timeout(Duration::from_secs(2))
            .expect("issued chunk claim result")
            .expect("issued chunk claim worker");
        let mut issue_before_restart = client
            .issue_chunk(issued_claim.chunk_id(), operation_id)
            .expect("issued chunk transition queues");
        let dropped_issued_permit = issue_before_restart
            .recv_timeout(Duration::from_secs(2))
            .expect("issued transition result")
            .expect("issued transition worker");
        let mut synthetic_write = client
            .begin_chunk_write(
                dropped_issued_permit,
                issued_claim.identity(),
                issued_claim.logical_length(),
            )
            .expect("synthetic write queues");
        synthetic_write
            .recv_timeout(Duration::from_secs(2))
            .expect("synthetic write result")
            .expect("synthetic write worker");
        assert_eq!(owner.synthetic_chunk_write_invocations(), 1);

        let stale_claim = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("stale ticket claim"),
            first.participant_id(),
            String::from("chunk-stale-ticket"),
            facts.allocation_unit * 5,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("stale ticket request");
        let mut stale_claim_request = client
            .claim_chunk(stale_claim.clone())
            .expect("stale ticket claim queues");
        stale_claim_request
            .recv_timeout(Duration::from_secs(2))
            .expect("stale ticket claim result")
            .expect("stale ticket claim worker");
        let mut stale_issue_request = client
            .issue_chunk(stale_claim.chunk_id(), operation_id)
            .expect("stale ticket issue queues");
        let stale_permit = stale_issue_request
            .recv_timeout(Duration::from_secs(2))
            .expect("stale ticket issue result")
            .expect("stale ticket issue worker");

        assert!(!issue_id.is_nil());

        assert!(owner.shutdown());
        let reopened_store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let mut reopened_owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            reopened_store,
            registry_path,
        )
        .expect("reopened journal owner");
        wait_until_available(&reopened_owner);
        let reopened_client = reopened_owner.client();
        let mut stale_entry = reopened_client
            .begin_chunk_write(
                stale_permit,
                stale_claim.identity(),
                stale_claim.logical_length(),
            )
            .expect("stale owner ticket queues");
        assert!(
            stale_entry
                .recv_timeout(Duration::from_secs(2))
                .expect("stale owner ticket result")
                .is_err()
        );
        assert_eq!(reopened_owner.synthetic_chunk_write_invocations(), 0);
        let mut reissue = reopened_client
            .issue_chunk(restart_claim.chunk_id(), operation_id)
            .expect("reissue Claimed chunk queues");
        let reissued_permit = reissue
            .recv_timeout(Duration::from_secs(2))
            .expect("reissue result")
            .expect("reissue worker");
        let mut reissued_release = reopened_client
            .release_chunk(reissued_permit, Uuid::new_v4())
            .expect("reissued release queues");
        reissued_release
            .recv_timeout(Duration::from_secs(2))
            .expect("reissued release result")
            .expect("reissued release worker");
        let mut issued_replay = reopened_client
            .issue_chunk(issued_claim.chunk_id(), operation_id)
            .expect("Issued replay queues");
        let issued_replay = issued_replay
            .recv_timeout(Duration::from_secs(2))
            .expect("Issued replay result")
            .expect_err("restart must not reissue a possibly escaped permit");
        assert!(format!("{issued_replay}").contains("already Issued"));
        assert!(reopened_owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn synthetic_chunk_entry_denials_never_invoke_callback() {
        let fixture = fixture("journal-owner-chunk-entry-callback");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        );
        let operation_id = {
            let mut request = owner
                .client()
                .admit_prepared(input)
                .expect("initial operation admission queues");
            request
                .recv_timeout(Duration::from_secs(2))
                .expect("initial operation result")
                .expect("initial operation worker")
        };
        let client = owner.client();
        let mut operation_lookup = client.get(operation_id).expect("operation lookup queues");
        let operation = operation_lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("operation lookup result")
            .expect("operation lookup worker")
            .expect("operation record");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let make_request = |key: &str, offset: u64| {
            ChunkClaimRequest::new(
                ChunkId::new(),
                operation_id,
                AdmissionClaim::new(
                    AdmissionClaimId::new(),
                    facts.identity.clone(),
                    crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                    facts.allocation_unit,
                )
                .expect("entry test claim"),
                participant_id,
                String::from(key),
                offset,
                facts.allocation_unit,
                facts.allocation_unit,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("entry test request")
        };
        let issue = |request: &ChunkClaimRequest| {
            let mut claim = client
                .claim_chunk(request.clone())
                .expect("entry test claim queues");
            claim
                .recv_timeout(Duration::from_secs(2))
                .expect("entry test claim result")
                .expect("entry test claim worker");
            let mut issue = client
                .issue_chunk(request.chunk_id(), operation_id)
                .expect("entry test issue queues");
            issue
                .recv_timeout(Duration::from_secs(2))
                .expect("entry test issue result")
                .expect("entry test issue worker")
        };

        let identity_request = make_request("entry-identity", 0);
        let identity_permit = issue(&identity_request);
        let before_identity = owner.synthetic_chunk_write_invocations();
        let mut identity_denied = client
            .begin_chunk_write(
                identity_permit,
                ChunkWriteIdentity::new(
                    identity_request.participant_id(),
                    String::from("different-output"),
                    identity_request.logical_offset(),
                    identity_request.logical_length(),
                )
                .expect("different identity"),
                identity_request.logical_length(),
            )
            .expect("identity denial queues");
        assert!(
            identity_denied
                .recv_timeout(Duration::from_secs(2))
                .expect("identity denial result")
                .is_err()
        );
        assert_eq!(owner.synthetic_chunk_write_invocations(), before_identity);

        let budget_request = make_request("entry-budget", facts.allocation_unit);
        let budget_permit = issue(&budget_request);
        let before_budget = owner.synthetic_chunk_write_invocations();
        let mut budget_denied = client
            .begin_chunk_write(
                budget_permit,
                budget_request.identity(),
                budget_request.logical_length() + 1,
            )
            .expect("budget denial queues");
        assert!(
            budget_denied
                .recv_timeout(Duration::from_secs(2))
                .expect("budget denial result")
                .is_err()
        );
        assert_eq!(owner.synthetic_chunk_write_invocations(), before_budget);

        let valid_request = make_request("entry-valid", facts.allocation_unit * 2);
        let valid_permit = issue(&valid_request);
        let mut valid = client
            .begin_chunk_write(
                valid_permit,
                valid_request.identity(),
                valid_request.logical_length(),
            )
            .expect("valid entry queues");
        valid
            .recv_timeout(Duration::from_secs(2))
            .expect("valid entry result")
            .expect("valid entry worker");
        assert_eq!(owner.synthetic_chunk_write_invocations(), before_budget + 1);

        assert!(owner.shutdown());
        drop(fixture);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn cancelled_operation_blocks_chunk_issue_and_entry_but_allows_unused_release() {
        let fixture = fixture("journal-owner-chunk-cancel");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let client = owner.client();

        let operation_id = OperationId::new();
        let prepared = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        );
        let record = prepared_record(operation_id, &prepared).expect("prepared record");
        let participant_id = record
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let volume = facts.identity.clone();
        let allocation_unit = facts.allocation_unit;
        let mut persist = client
            .persist_intent(record)
            .expect("persist cancelled operation");
        let receipt = persist
            .recv_timeout(Duration::from_secs(2))
            .expect("persist result")
            .expect("persist worker")
            .receipt()
            .expect("persist receipt");
        let first_request = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                allocation_unit,
            )
            .expect("first cancellation claim"),
            participant_id,
            String::from("cancelled-claimed"),
            0,
            allocation_unit,
            allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("first cancellation request");
        let mut claim = client
            .claim_chunk(first_request.clone())
            .expect("claim before cancellation");
        claim
            .recv_timeout(Duration::from_secs(2))
            .expect("claim result")
            .expect("claim worker");
        let mut cancel = client
            .request_cancel_before_publish(
                operation_id,
                receipt.sequence(),
                CancellationRequestObservation::new(2).expect("cancel observation"),
            )
            .expect("cancel request");
        cancel
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel result")
            .expect("cancel worker");
        let mut issue_after_claim_cancel = client
            .issue_chunk(first_request.chunk_id(), operation_id)
            .expect("issue after claimed cancellation");
        assert!(
            issue_after_claim_cancel
                .recv_timeout(Duration::from_secs(2))
                .expect("issue after claimed cancellation result")
                .is_err()
        );
        let mut cancel_claim = client
            .cancel_chunk(first_request.chunk_id(), operation_id, Uuid::new_v4())
            .expect("cancel claimed chunk");
        cancel_claim
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel claimed chunk result")
            .expect("cancel claimed chunk worker");

        let issued_operation_id = OperationId::new();
        let issued_prepared = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            volume.clone(),
            allocation_unit,
        );
        let issued_record =
            prepared_record(issued_operation_id, &issued_prepared).expect("issued record");
        let issued_participant_id = issued_record
            .universal_intent
            .as_ref()
            .expect("issued universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let mut persist_issued = client
            .persist_intent(issued_record)
            .expect("persist issued operation");
        let issued_receipt = persist_issued
            .recv_timeout(Duration::from_secs(2))
            .expect("persist issued result")
            .expect("persist issued worker")
            .receipt()
            .expect("issued persist receipt");
        let issued_request = ChunkClaimRequest::new(
            ChunkId::new(),
            issued_operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume.clone(),
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                allocation_unit,
            )
            .expect("issued cancellation claim"),
            issued_participant_id,
            String::from("cancelled-issued"),
            allocation_unit,
            allocation_unit,
            allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("issued cancellation request");
        let mut issued_claim = client
            .claim_chunk(issued_request.clone())
            .expect("issued claim");
        issued_claim
            .recv_timeout(Duration::from_secs(2))
            .expect("issued claim result")
            .expect("issued claim worker");
        let mut issue = client
            .issue_chunk(issued_request.chunk_id(), issued_operation_id)
            .expect("issue before cancellation");
        let permit = issue
            .recv_timeout(Duration::from_secs(2))
            .expect("issue result")
            .expect("issue worker");
        let mut cancel_issued = client
            .request_cancel_before_publish(
                issued_operation_id,
                issued_receipt.sequence(),
                CancellationRequestObservation::new(3).expect("issued cancel observation"),
            )
            .expect("issued cancel request");
        let issued_cancel_receipt = cancel_issued
            .recv_timeout(Duration::from_secs(2))
            .expect("issued cancel result")
            .expect("issued cancel worker");
        let mut denied_entry = client
            .begin_chunk_write(
                permit,
                issued_request.identity(),
                issued_request.logical_length(),
            )
            .expect("entry after cancellation");
        assert!(
            denied_entry
                .recv_timeout(Duration::from_secs(2))
                .expect("entry after cancellation result")
                .is_err()
        );
        assert_eq!(owner.synthetic_chunk_write_invocations(), 0);

        assert!(owner.shutdown());
        let restarted = OperationJournalStore::open(&fixture.owner).expect("restart journal");
        let cancelled = restarted
            .get(issued_operation_id)
            .expect("read cancelled operation")
            .expect("retained cancelled operation");
        assert_eq!(
            cancelled.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(
            cancelled.cancellation_request(),
            Some(&CancellationRequestObservation::new(3).expect("cancel observation"))
        );
        assert_eq!(
            restarted
                .request_cancel_before_publish(
                    issued_operation_id,
                    issued_receipt.sequence(),
                    CancellationRequestObservation::new(3).expect("replay observation"),
                )
                .expect("same request is read-only after restart"),
            issued_cancel_receipt
        );
        assert_eq!(
            restarted
                .read_chunk_claim(issued_request.chunk_id())
                .expect("read issued claim after restart")
                .expect("issued claim retained")
                .state(),
            ChunkClaimState::Issued
        );
        let rebuild = restarted
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild cancellation charges");
        assert_eq!(rebuild.readiness(), &CapacityRebuildReadiness::Ready);
        let rebuilt_provider = CapacityProvider::unavailable();
        rebuilt_provider
            .install_rebuilt_claims(&rebuild)
            .expect("install retained cancellation charge");
        assert_eq!(
            rebuilt_provider.outstanding(
                &volume,
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            ),
            allocation_unit
        );
        assert!(restarted.prove_clean_handoff_durable().is_err());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_pending_chunk_release_retry_reuses_exact_proof_without_reentry() {
        let fixture = fixture("journal-owner-chunk-release-pending");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);
        let observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("descriptor observation");
        let (volume, facts) = observation
            .facts
            .iter()
            .next()
            .map(|(volume, facts)| (volume.clone(), facts.clone()))
            .expect("profile facts");
        runtime.capacity_provider = Some(CapacityProvider::unavailable());
        let operation_id = runtime
            .admit_prepared(prepared_copy_input(
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                volume.clone(),
                facts.allocation_unit,
            ))
            .expect("initial operation admission");
        let operation = runtime
            .store()
            .get(operation_id)
            .expect("read operation")
            .expect("operation row");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let request = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume,
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("chunk claim"),
            participant_id,
            String::from("pending-release"),
            0,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("chunk request");
        runtime
            .claim_chunk(request.clone())
            .expect("persist chunk claim");
        let permit = runtime
            .issue_chunk(request.chunk_id(), operation_id)
            .expect("issue chunk");
        let issue_id = permit.issue_id();
        let release_id = Uuid::new_v4();
        runtime.store().set_release_test_hook(|boundary| {
            if boundary == ReleaseTestBoundary::BeforePrecommitValidation {
                ReleaseTestDirective::Fail(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("forced owner release rollback"),
                })
            } else {
                ReleaseTestDirective::Continue
            }
        });
        let first_error = runtime
            .release_chunk(permit, release_id)
            .expect_err("confirmed release rollback retains pending proof");
        assert!(format!("{first_error}").contains("forced owner release rollback"));
        assert_eq!(
            runtime
                .pending_chunk_releases
                .get(&release_id)
                .map(|pending| pending.issue_id),
            Some(issue_id)
        );
        assert_eq!(
            runtime
                .store()
                .read_chunk_claim(request.chunk_id())
                .expect("read retained chunk")
                .expect("retained chunk")
                .state(),
            ChunkClaimState::Issued
        );
        assert_eq!(
            runtime
                .synthetic_chunk_write_invocations
                .load(Ordering::Relaxed),
            0
        );
        let replay = runtime
            .retry_chunk_release(request.chunk_id(), operation_id, release_id)
            .expect("retry exact pending release");
        assert_eq!(replay.release_id(), release_id);
        assert!(!runtime.pending_chunk_releases.contains_key(&release_id));
        assert!(matches!(
            runtime.issue_chunk(request.chunk_id(), operation_id),
            Err(OperationJournalOwnerError::ChunkClaim { reason })
                if reason.contains("released")
        ));
        let durable_replay = runtime
            .retry_chunk_release(request.chunk_id(), operation_id, release_id)
            .expect("durable release replay");
        assert_eq!(durable_replay, replay);
        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stale_second_chunk_witness_retains_issued_charge_without_panicking() {
        let fixture = fixture("journal-owner-chunk-stale-entry-witness");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut runtime = OwnerRuntime::new(store).expect("owner runtime");
        let volume_owner = runtime
            .store()
            .acquire_profile_journal_volume_owner(Some(&registry_path))
            .expect("profile volume owner");
        runtime.install_volume_owner(volume_owner);
        let reserve = runtime
            .store()
            .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry_path))
            .expect("profile reserve");
        runtime.install_reserve(reserve);
        let observation = runtime
            .refresh_live_descriptor_capacity()
            .expect("descriptor observation");
        let (volume, facts) = observation
            .facts
            .iter()
            .next()
            .map(|(volume, facts)| (volume.clone(), facts.clone()))
            .expect("profile facts");
        runtime.capacity_provider = Some(CapacityProvider::unavailable());
        let operation_id = runtime
            .admit_prepared(prepared_copy_input(
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                volume.clone(),
                facts.allocation_unit,
            ))
            .expect("initial operation admission");
        let operation = runtime
            .store()
            .get(operation_id)
            .expect("read operation")
            .expect("operation row");
        let participant_id = operation
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        let request = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume,
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.allocation_unit,
            )
            .expect("chunk claim"),
            participant_id,
            String::from("stale-entry-witness"),
            0,
            facts.allocation_unit,
            facts.allocation_unit,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("chunk request");
        runtime
            .claim_chunk(request.clone())
            .expect("persist chunk claim");
        let permit = runtime
            .issue_chunk(request.chunk_id(), operation_id)
            .expect("issue chunk");
        let issue_id = permit.issue_id();
        runtime.fail_next_chunk_callback_witness_for_test("forced stale source authority");

        let error = runtime
            .begin_chunk_write(permit, request.identity(), request.logical_length())
            .expect_err("stale callback witness must be typed");
        assert!(format!("{error}").contains("forced stale source authority"));
        assert_eq!(
            runtime
                .synthetic_chunk_write_invocations
                .load(Ordering::Relaxed),
            0
        );
        let retained = runtime
            .store()
            .read_chunk_claim(request.chunk_id())
            .expect("read retained chunk")
            .expect("retained chunk");
        assert_eq!(retained.state(), ChunkClaimState::Issued);
        assert_eq!(retained.issue_id(), Some(issue_id));

        // The owner continues to service live capacity work after the stale authority denial;
        // the failed entry has neither panicked the worker nor erased its durable charge.
        runtime
            .refresh_live_descriptor_capacity()
            .expect("owner remains usable after stale witness");
        drop(runtime);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_persists_all_full_capacity_classes_through_owner_queue() {
        let fixture = fixture("journal-owner-prepared-full-only");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_all_classes_input(facts.identity, facts.allocation_unit);
        let expected_plan = input.plan.clone();
        let expected_claims = input.claims.clone();
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("full-only admission queues");
        let operation_id = request
            .recv_timeout(Duration::from_secs(2))
            .expect("full-only admission result")
            .expect("full-only admission commits");
        let mut lookup = owner
            .client()
            .get(operation_id)
            .expect("full-only lookup queues");
        let record = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("full-only lookup result")
            .expect("full-only record exists")
            .expect("full-only record present");
        assert_eq!(record.schema_version, crate::operation_journal::SCHEMA_V16);
        assert!(record.capacity_plan.is_none());
        assert_eq!(
            record
                .full_capacity_plan
                .as_ref()
                .expect("full plan persisted")
                .allocations()
                .len(),
            9
        );
        assert_eq!(
            record
                .full_capacity_plan
                .as_ref()
                .expect("full plan persisted")
                .allocations()
                .iter()
                .map(|allocation| allocation.class())
                .find(|class| {
                    *class
                        == crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload
                }),
            Some(
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload
            )
        );
        assert!(owner.shutdown());
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let reopened_record = reopened
            .get(operation_id)
            .expect("reopen V16 record")
            .expect("reopened V16 record exists");
        assert_eq!(
            reopened_record.schema_version,
            crate::operation_journal::SCHEMA_V16
        );
        assert_eq!(
            reopened_record
                .full_capacity_plan
                .as_ref()
                .expect("reopened full plan"),
            &expected_plan
        );
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild all full-class claims");
        assert_eq!(snapshot.claims().len(), expected_claims.claims().len());
        assert!(matches!(
            snapshot.readiness(),
            CapacityRebuildReadiness::Ready
        ));
        let mut expected_claim_rows: Vec<_> = expected_claims
            .claims()
            .iter()
            .map(|claim| {
                (
                    claim.claim_id(),
                    claim.volume().clone(),
                    claim.allocation_class(),
                    claim.allocated_bytes(),
                )
            })
            .collect();
        let mut rebuilt_claim_rows: Vec<_> = snapshot
            .claims()
            .iter()
            .map(|claim| {
                (
                    claim.claim_id(),
                    claim.volume().clone(),
                    claim.allocation_class(),
                    claim.allocated_bytes(),
                )
            })
            .collect();
        expected_claim_rows.sort_by_key(|row| row.0);
        rebuilt_claim_rows.sort_by_key(|row| row.0);
        assert_eq!(rebuilt_claim_rows, expected_claim_rows);

        let rebuilt_provider = CapacityProvider::unavailable();
        rebuilt_provider
            .install_rebuilt_claims(&snapshot)
            .expect("install exact restarted claim image");
        assert_eq!(rebuilt_provider.claim_count(), 1);
        assert_eq!(rebuilt_provider.ledger_claims(), expected_claim_rows);
        for claim in expected_claims.claims() {
            assert_eq!(
                rebuilt_provider.outstanding(claim.volume(), claim.allocation_class()),
                claim.allocated_bytes()
            );
        }
        drop(reopened);
        drop(fixture);
    }

    #[test]
    fn prepared_admission_rejects_oversized_record_before_id_allocation() {
        let (_fixture, owner) = open_owner("journal-owner-prepared-record-bounds");
        let mut input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        );
        input.intent.label = "x".repeat(crate::operation_journal::MAX_UNIVERSAL_LABEL_BYTES + 1);
        let before = owner.prepared_admission_id_allocations();
        assert!(matches!(
            owner.client().admit_prepared(input),
            Err(OperationJournalOwnerQueueError::InvalidInput { reason })
                if reason.contains("label")
        ));
        assert_eq!(
            owner.prepared_admission_id_allocations(),
            before,
            "record validation must not mint an operation identity"
        );
        drop(owner);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_owner_queue_faults_are_typed_and_side_effect_free() {
        const SEED: u64 = 0x1567;

        let fixture = fixture("journal-owner-prepared-faults");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let cases = [
            (
                PreparedAdmissionFactsFault::LowReserve,
                "recovery reserve is low",
            ),
            (
                PreparedAdmissionFactsFault::DiskPressure,
                "prepared admission requires",
            ),
            (
                PreparedAdmissionFactsFault::DiskPressureBoundary,
                "prepared admission requires",
            ),
            (
                PreparedAdmissionFactsFault::Unsupported,
                "descriptor refresh was rejected",
            ),
            (
                PreparedAdmissionFactsFault::Replaced,
                "descriptor refresh was rejected",
            ),
        ];
        for (fault, expected) in cases {
            let before_capacity = {
                let mut request = owner
                    .client()
                    .inspect_capacity_for_test()
                    .expect("capacity snapshot queues");
                request
                    .recv_timeout(Duration::from_secs(2))
                    .expect("capacity snapshot result")
                    .expect("capacity snapshot worker")
            };
            let before_id = owner.prepared_admission_id_allocations();
            let input = prepared_copy_input(
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
                facts.identity.clone(),
                facts.allocation_unit,
            )
            .with_facts_fault(fault);
            let mut request = owner
                .client()
                .admit_prepared(input)
                .expect("faulted admission queues");
            let result = request
                .recv_timeout(Duration::from_secs(2))
                .expect("faulted admission result")
                .expect_err("faulted admission must reject");
            assert!(
                format!("{result}").contains(expected),
                "unexpected typed fault result: {result:?}"
            );
            assert_eq!(
                owner.prepared_admission_id_allocations(),
                before_id,
                "descriptor rejection must precede operation identity allocation"
            );
            let after_capacity = {
                let mut request = owner
                    .client()
                    .inspect_capacity_for_test()
                    .expect("capacity snapshot queues after rejection");
                request
                    .recv_timeout(Duration::from_secs(2))
                    .expect("capacity snapshot after rejection result")
                    .expect("capacity snapshot after rejection worker")
            };
            assert_eq!(
                after_capacity, before_capacity,
                "seed {SEED:#x} pre-intent fault {fault:?} must leave the owner ledger and token sequence unchanged"
            );
        }
        let mut recovery = owner
            .client()
            .scan_recovery_page(None)
            .expect("recovery scan queues");
        assert!(
            recovery
                .recv_timeout(Duration::from_secs(2))
                .expect("recovery scan result")
                .expect("recovery scan worker")
                .entries()
                .is_empty()
        );
        assert!(owner.shutdown());
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild rejected-admission snapshot");
        assert!(snapshot.claims().is_empty());
        assert!(
            reopened
                .scan_recovery_page(None)
                .expect("recovery page after reopen")
                .entries()
                .is_empty()
        );
        drop(reopened);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_owner_queue_accepts_exact_free_floor_plus_required_boundary() {
        let fixture = fixture("journal-owner-prepared-exact-boundary");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        )
        .with_facts_fault(PreparedAdmissionFactsFault::ExactBoundary);
        let expected_claim = input.claims.claims()[0].claim_id();
        let before_id = owner.prepared_admission_id_allocations();
        let before_capacity = {
            let mut request = owner
                .client()
                .inspect_capacity_for_test()
                .expect("capacity snapshot queues");
            request
                .recv_timeout(Duration::from_secs(2))
                .expect("capacity snapshot result")
                .expect("capacity snapshot worker")
        };
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("exact-boundary admission queues");
        let operation_id = request
            .recv_timeout(Duration::from_secs(2))
            .expect("exact-boundary admission result")
            .expect("exact floor plus required bytes must admit");
        assert_eq!(
            owner.prepared_admission_id_allocations(),
            before_id + 1,
            "the operation identity is allocated only after the exact boundary passes"
        );
        let after_capacity = {
            let mut request = owner
                .client()
                .inspect_capacity_for_test()
                .expect("capacity snapshot queues after admission");
            request
                .recv_timeout(Duration::from_secs(2))
                .expect("capacity snapshot after admission result")
                .expect("capacity snapshot after admission worker")
        };
        assert_eq!(after_capacity.next_token, before_capacity.next_token + 1);
        assert_eq!(after_capacity.claims.len(), 1);
        assert_eq!(after_capacity.claims[0].0, expected_claim);
        assert_eq!(after_capacity.claims[0].1, facts.identity);
        assert_eq!(
            after_capacity.claims[0].2,
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging
        );
        assert_eq!(after_capacity.claims[0].3, facts.allocation_unit);
        let mut lookup = owner
            .client()
            .get(operation_id)
            .expect("accepted operation lookup queues");
        assert!(
            lookup
                .recv_timeout(Duration::from_secs(2))
                .expect("accepted operation lookup result")
                .expect("accepted operation lookup worker")
                .is_some()
        );
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_rejects_short_retained_reserve_before_id_allocation() {
        let fixture = fixture("journal-owner-prepared-short-reserve");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);

        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        drop(descriptor);
        fs::OpenOptions::new()
            .write(true)
            .open(&reserve_path)
            .expect("open reserve for short-reserve fixture")
            .set_len(PROTECTED_FREE_SPACE_FLOOR - 1)
            .expect("shorten reserve descriptor");

        let before_id = owner.prepared_admission_id_allocations();
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload,
            facts.identity,
            facts.allocation_unit,
        );
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("short-reserve admission queues");
        let result = request
            .recv_timeout(Duration::from_secs(2))
            .expect("short-reserve admission result")
            .expect_err("short retained reserve must reject");
        assert!(matches!(
            result,
            OperationJournalOwnerError::PreparedAdmission(
                PreparedAdmissionError::DescriptorRefreshRejected { .. }
            )
        ));
        assert_eq!(
            owner.prepared_admission_id_allocations(),
            before_id,
            "full physical-floor rejection must precede operation identity allocation"
        );
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_owner_queue_preserves_uncertain_settlement_identity() {
        use super::super::store::{InsertTestBoundary, InsertTestDirective};

        let fixture = fixture("journal-owner-prepared-uncertain");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("prepared admission commit fault"),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload,
            facts.identity,
            facts.allocation_unit,
        );
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("uncertain admission queues");
        let result = request
            .recv_timeout(Duration::from_secs(2))
            .expect("uncertain admission result")
            .expect_err("uncertain settlement must be surfaced");
        assert!(matches!(
            result,
            OperationJournalOwnerError::PreparedAdmissionRetained(
                PreparedAdmissionRetainedError::Uncertain {
                    stage: JournalInsertWriteStage::CommitOutcomeUnknown,
                    claim_ids,
                    ..
                }
            ) if claim_ids.len() == 1
        ));
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_owner_queue_reaudits_recovery_after_startup() {
        let fixture = fixture("journal-owner-prepared-recovery-audit");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let first_input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
        );
        let mut first_request = owner
            .client()
            .admit_prepared(first_input)
            .expect("first admission queues");
        let first_operation = first_request
            .recv_timeout(Duration::from_secs(2))
            .expect("first admission result")
            .expect("first admission commits");

        let mut recovery_request = owner
            .client()
            .inject_active_recovery_charge_for_test(first_operation, facts.identity.clone(), 4096)
            .expect("inject active recovery evidence");
        recovery_request
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery evidence result")
            .expect("insert active recovery evidence");

        let before_id = owner.prepared_admission_id_allocations();
        let second_input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity,
            facts.allocation_unit,
        );
        let mut second_request = owner
            .client()
            .admit_prepared(second_input)
            .expect("recovery-blocked admission queues");
        let result = second_request
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery-blocked admission result")
            .expect_err("active recovery evidence must block admission");
        assert!(matches!(
            result,
            OperationJournalOwnerError::PreparedAdmission(
                PreparedAdmissionError::RecoveryRequired { .. }
            )
        ));
        assert_eq!(
            owner.prepared_admission_id_allocations(),
            before_id,
            "recovery audit must reject before operation identity allocation"
        );
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_owner_queue_reports_confirmed_rollback_distinctly() {
        use super::super::store::{InsertTestBoundary, InsertTestDirective};

        let fixture = fixture("journal-owner-prepared-rollback");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("prepared admission rollback fault"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let input = prepared_copy_input(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload,
            facts.identity,
            facts.allocation_unit,
        );
        let before = owner.prepared_admission_id_allocations();
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("rollback admission queues");
        let result = request
            .recv_timeout(Duration::from_secs(2))
            .expect("rollback admission result")
            .expect_err("confirmed rollback must be surfaced");
        assert!(matches!(
            result,
            OperationJournalOwnerError::PreparedAdmissionRolledBack(_)
        ));
        assert_eq!(
            owner.prepared_admission_id_allocations(),
            before + 1,
            "rollback is reported only after the post-ID persistence boundary"
        );
        assert!(owner.shutdown());
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_admission_uses_fresh_owner_facts_and_settles_once() {
        let fixture = fixture("journal-owner-prepared-success");
        let registry_path = fixture_volume_registry(&fixture);
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let mut owner =
            OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry_path)
                .expect("journal owner");
        wait_until_available(&owner);
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("operation_journal")
            .join(super::super::capacity_reserve_namespace::RESERVE_FILE_NAME);
        let descriptor = fs::File::open(&reserve_path).expect("reserve descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("reserve facts");
        let claim_id = crate::operation_journal::AdmissionClaimId::new();
        let input = prepared_copy_input_with_claim_id(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
            claim_id,
        );
        let mut request = owner
            .client()
            .admit_prepared(input)
            .expect("prepared admission queues");
        let operation_id = request
            .recv_timeout(Duration::from_secs(2))
            .expect("prepared admission result")
            .expect("prepared admission commits");
        let duplicate = prepared_copy_input_with_claim_id(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
            claim_id,
        );
        let mut duplicate_request = owner
            .client()
            .admit_prepared(duplicate)
            .expect("duplicate admission queues");
        let duplicate_result = duplicate_request
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate result")
            .expect_err("duplicate claim must be rejected");
        assert!(matches!(
            duplicate_result,
            OperationJournalOwnerError::PreparedAdmission(
                PreparedAdmissionError::DuplicateClaim { .. }
            )
        ));

        let second_input = prepared_copy_input_with_claim_id(
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            facts.identity.clone(),
            facts.allocation_unit,
            crate::operation_journal::AdmissionClaimId::new(),
        );
        let mut second_request = owner
            .client()
            .admit_prepared(second_input)
            .expect("coexisting admission queues");
        let second_operation = second_request
            .recv_timeout(Duration::from_secs(2))
            .expect("coexisting admission result")
            .expect("active ordinary durable claim may coexist");
        assert_ne!(second_operation, operation_id);
        assert!(owner.shutdown());

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen journal");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild ordinary claims");
        assert!(
            snapshot
                .claims()
                .iter()
                .any(|claim| claim.operation_id() == operation_id)
        );
        assert!(
            snapshot
                .claims()
                .iter()
                .any(|claim| claim.operation_id() == second_operation)
        );
        drop(reopened);
        drop(fixture);
    }

    fn v6_import_record(operation_id: OperationId) -> (StoredOperationRecord, ImportIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Import,
            "operation.status.owner-import",
            participants,
        )
        .expect("import universal intent");
        let import_args = ImportIntentArgs::new(
            ExternalRootBindingId::new(),
            filesystem_participant_id,
            "owner-external-root",
            StoredRelativePath::new("sample.wav").expect("external path"),
            ImportFileExpectation::new("owner-external-file", 10, 11)
                .expect("external file expectation"),
            copy_args.destination().clone(),
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/owner-import")),
        )
        .expect("import arguments");
        let record = StoredOperationRecord::new_v6_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::Import(import_args.clone()),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, import_args)
    }

    fn v7_waveform_edit_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, WaveformEditIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::DestructiveEdit,
            "operation.status.owner-waveform-edit",
            participants,
        )
        .expect("waveform edit universal intent");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("waveform source spec");
        let edit_args = WaveformEditIntentArgs::new(
            copy_args.source().clone(),
            WaveformFileRevisionObservation::new(
                "owner-file-identity",
                "owner-change-marker",
                1024,
                Some(12_000_000_000),
            )
            .expect("waveform file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("waveform edit range"),
            WaveformEditOperationArgs::GainAndFades(
                WaveformEditSettingsArgs::new(
                    FiniteF32Bits::new(-0.0).expect("signed zero gain"),
                    None,
                    None,
                )
                .expect("waveform edit settings"),
            ),
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("waveform edit arguments");
        let record = StoredOperationRecord::new_v7_waveform_edit(
            operation_id,
            copy_record.intent,
            universal_intent,
            edit_args.clone(),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, edit_args)
    }

    fn v8_namespace_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, NamespaceCommandArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let source_participant = copy_args.source().participant_id();
        let source_id = SourceId::from_string("owner-source");
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("in.wav").expect("namespace source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "owner-file", 10, Some(-2))
                    .expect("namespace file expectation"),
            ),
        )
        .expect("namespace source");
        let parent = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "owner-root", Some(-1))
                .expect("namespace root expectation"),
        )
        .expect("namespace parent");
        let command_args = NamespaceCommandArgs::Rename(
            NamespaceRenameIntentArgs::new(
                source,
                NamespaceTarget::new(
                    parent,
                    NamespaceLeafName::new("out.wav").expect("namespace leaf"),
                )
                .expect("namespace target"),
            )
            .expect("namespace rename"),
        );
        let universal_intent = UniversalIntent::new(
            None,
            command_args.command_kind(),
            "operation.status.owner-namespace",
            participants,
        )
        .expect("namespace universal intent");
        let record = StoredOperationRecord::new_v8_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::Namespace(command_args.clone()),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, command_args)
    }

    fn v9_duplicate_double_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, WaveformDuplicateDoubleIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 8, WaveformSampleFormat::Integer)
            .expect("render source spec");
        let command_args = WaveformDuplicateDoubleIntentArgs::new(
            copy_args.source().clone(),
            copy_args.destination().clone(),
            WaveformFileRevisionObservation::new(
                "owner-render-file",
                "owner-render-change",
                1024,
                Some(12_000_000_000),
            )
            .expect("render file revision"),
            source_spec,
            10,
            WaveformRenderOutputSpec::from_source(source_spec),
            CopyCollisionPolicy::NoReplace,
        )
        .expect("duplicate-double arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Duplicate,
            "operation.status.owner-render",
            participants,
        )
        .expect("render universal intent");
        let record = StoredOperationRecord::new_v9_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::Render(
                super::super::render_commands::RenderCommandArgs::DuplicateDouble(
                    command_args.clone(),
                ),
            ),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, command_args)
    }

    fn v10_trash_record(operation_id: OperationId) -> (StoredOperationRecord, TrashIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let source_participant = copy_args.source().participant_id();
        let filesystem_participant = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            SourceId::from_string("owner-source"),
            StoredRelativePath::new("folder/item.wav").expect("trash source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "owner-file", 10, Some(11))
                    .expect("trash file expectation"),
            ),
        )
        .expect("trash source endpoint");
        let trash_args = TrashIntentArgs::new(
            source,
            ExternalRootBindingId::new(),
            filesystem_participant,
            "owner-trash-root",
        )
        .expect("trash arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Trash,
            "operation.status.owner-trash",
            participants,
        )
        .expect("trash universal intent");
        let record = StoredOperationRecord::new_v10_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(trash_args.clone())),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, trash_args)
    }

    fn v11_retained_restore_record(operation_id: OperationId) -> StoredOperationRecord {
        let (v10_record, _) = v10_trash_record(operation_id);
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode(&v10_record).expect("encode V10 owner fixture"))
                .expect("parse V10 owner fixture");
        let source_participant =
            value["universal_intent"]["participants"]["descriptors"][0]["id"].clone();
        value["schema_version"] = serde_json::Value::from(SCHEMA_V11);
        value["universal_intent"]["command_kind"] = serde_json::Value::from("RestoreRetained");
        value["universal_command_args"] = serde_json::json!({
            "RetainedResolution": {
                "Restore": {
                    "observation": {
                        "original_relative": "folder/item",
                        "staged": {
                            "participant_id": source_participant,
                            "source_id": "owner-source",
                            "relative_path": ".yield_delete_staging/folder/item",
                            "expectation": {
                                "Directory": {
                                    "source_revision": 7,
                                    "filesystem_identity": "owner-staged-directory",
                                    "modified_unix_ns": null
                                }
                            }
                        },
                        "staged_relative": "folder/item",
                        "retained_id": "01234567-89ab-cdef-0123-456789abcdef",
                        "stage": "Deleted",
                        "recovery_root_kind": "LegacyDeleteStaging",
                        "selected_entry_fingerprint": [1, 2, 3, 4, 5, 6, 7, 8,
                            9, 10, 11, 12, 13, 14, 15, 16,
                            17, 18, 19, 20, 21, 22, 23, 24,
                            25, 26, 27, 28, 29, 30, 31, 32]
                    }
                }
            }
        });
        decode(&serde_json::to_vec(&value).expect("serialize V11 owner fixture"))
            .expect("decode V11 owner fixture")
    }

    fn v12_selection_normalize_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, SelectionNormalizeIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("selection source spec");
        let selection_args = SelectionNormalizeIntentArgs::new(
            copy_args.source().clone(),
            WaveformFileRevisionObservation::new(
                "owner-selection-file",
                "owner-selection-change",
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
            "operation.status.owner-selection-normalize",
            participants,
        )
        .expect("selection Normalize universal intent");
        let record = StoredOperationRecord::new_v12_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::SelectionNormalize(selection_args.clone()),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, selection_args)
    }

    fn v13_extract_and_trim_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, ExtractAndTrimIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("selection output source spec");
        let extraction = SelectionExtractIntentArgs::new(
            copy_args.source().clone(),
            7,
            WaveformFileRevisionObservation::new(
                "owner-selection-output-file",
                "owner-selection-output-change",
                1024,
                Some(-12_000_000_000),
            )
            .expect("selection output file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection output range"),
            SelectionOutputRecipe::RawWav(
                RawWavExtractionRecipe::new(
                    16,
                    FiniteF32Bits::new(1.0).expect("selection output gain"),
                    2,
                )
                .expect("selection output recipe"),
            ),
            SourcePlaybackType::OneShot,
            copy_args.destination().clone(),
            CopyCollisionPolicy::NoReplace,
        )
        .expect("selection output extraction");
        let command_args =
            ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                .expect("ExtractAndTrim arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::DestructiveEdit,
            "operation.status.owner-selection-output",
            participants,
        )
        .expect("selection output universal intent");
        let record = StoredOperationRecord::new_v13_command(
            operation_id,
            copy_record.intent,
            universal_intent,
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
                command_args.clone(),
            )),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, command_args)
    }

    fn v14_history_record(
        operation_id: OperationId,
    ) -> (StoredOperationRecord, HistoryUndoRedoIntentArgs) {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let revision = |identity: &str| {
            WaveformFileRevisionObservation::new(identity, "owner-history-change", 10, Some(11))
                .expect("history revision")
        };
        let backup = HistoryRecoveryBackupObservation::new(
            filesystem_participant_id,
            "owner-history-root",
            uuid::Uuid::from_u128(20),
            "owner-history-directory",
            Some(HistoryRecoveryLeafObservation::new(revision("before")).expect("history backup")),
            None,
            None,
        )
        .expect("history backup observation");
        let restore = HistoryWaveformRestore::new(
            copy_args.source().clone(),
            7,
            HistoryRestoreTargetExpectation::Existing(revision("target")),
            Some(String::from("owner-before-content")),
            backup,
            None,
        )
        .expect("history restore");
        let command_args = HistoryUndoRedoIntentArgs::new(
            HistoryScopeId::from_uuid(uuid::Uuid::from_u128(30)),
            4,
            5,
            Some(6),
            HistoryDirection::Undo,
            vec![HistoryAction::WaveformRestore(restore)],
        )
        .expect("history arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Undo,
            "operation.status.owner-history",
            participants,
        )
        .expect("history universal intent");
        let intent = OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileHistory,
            label: String::from("owner history"),
        };
        let record = StoredOperationRecord::new_v14_command(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::History(command_args.clone()),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        );
        (record, command_args)
    }

    fn v15_external_record(operation_id: OperationId) -> StoredOperationRecord {
        let (copy_record, copy_args) = v5_copy_record(operation_id);
        let participants = copy_record
            .universal_intent
            .as_ref()
            .expect("copy universal intent")
            .participants()
            .clone();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let provenance = ExternalRawObservationProvenance::new(
            SourceId::from_string("owner-source"),
            Some(vec![1, 2]),
            None,
            4,
            ExternalCaptureBoundary::new(50, None, None).expect("capture boundary"),
        )
        .expect("external provenance");
        let observation = ExternalRawObservation::new(
            ExternalRawEventKind::Error,
            vec![
                ExternalRawObservedPath::new(
                    ExternalNativeString::unix_bytes(b"changed.wav".to_vec()).expect("native path"),
                    ExternalRawPathRole::Subject,
                    None,
                )
                .expect("path"),
            ],
            ExternalRawObservationMetadata::new().with_flags(1),
            1 << 14,
        )
        .expect("external observation");
        let raw = ExternalRawObservationEnvelope::new(
            provenance,
            ExternalRawObservationLimits::new(64, 4096, 4096).expect("raw limits"),
            ExternalRawNativeAccounting::new(1, 12, 9),
            vec![observation],
        )
        .expect("external envelope");
        let args = ExternalReconciliationIntentArgs::new(
            EvidenceBatchId::from_uuid(uuid::Uuid::from_u128(44)),
            copy_args.source().participant_id(),
            filesystem_participant_id,
            7,
            raw,
        )
        .expect("external command arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::ExternalReconciliation,
            "operation.status.owner-external",
            participants,
        )
        .expect("external universal intent");
        let intent = OperationIntent {
            actor: OperationActor::ExternalFs,
            kind: OperationKind::ExternalReconciliation,
            label: String::from("owner external"),
        };
        StoredOperationRecord::new_v15_command(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::ExternalReconciliation(args),
            copy_record.capacity_plan.expect("capacity plan"),
            50,
        )
    }

    #[test]
    fn owner_persists_and_reads_through_cloneable_client() {
        let (_fixture, mut owner) = open_owner("journal-owner-basic");
        let client = owner.client();
        let operation_id = OperationId::new();
        let mut request = client
            .persist_intent(intent_record(operation_id))
            .expect("persist admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("persist result")
            .expect("worker result");
        let receipt = outcome.receipt().expect("acknowledged insert");
        assert_eq!(receipt.operation_id(), operation_id);
        let mut lookup = client.get(operation_id).expect("lookup admission");
        assert_eq!(
            lookup
                .recv_timeout(Duration::from_secs(2))
                .expect("lookup result")
                .expect("lookup worker result")
                .map(|record| record.operation_id),
            Some(operation_id)
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v6_import_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v6-import");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, import_args) = v6_import_record(operation_id);
        let mut request = client.persist_intent(record).expect("import admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("import result")
            .expect("import worker result");
        let receipt = outcome.receipt().expect("import acknowledgement");
        assert_eq!(receipt.operation_id(), operation_id);

        let mut lookup = client.get(operation_id).expect("import lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("import lookup result")
            .expect("import lookup worker result")
            .expect("persisted import");
        assert_eq!(persisted.schema_version, SCHEMA_V6);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Import(import_args))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("import universal intent")
                .command_kind(),
            UniversalCommandKind::Import
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v7_waveform_edit_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v7-waveform-edit");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, edit_args) = v7_waveform_edit_record(operation_id);
        let mut request = client.persist_intent(record).expect("edit admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("edit result")
            .expect("edit worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("edit acknowledgement")
                .operation_id(),
            operation_id
        );

        let mut lookup = client.get(operation_id).expect("edit lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("edit lookup result")
            .expect("edit lookup worker result")
            .expect("persisted edit");
        assert_eq!(persisted.schema_version, SCHEMA_V7);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::WaveformEdit(edit_args))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("edit universal intent")
                .command_kind(),
            UniversalCommandKind::DestructiveEdit
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v8_namespace_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v8-namespace");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, namespace_args) = v8_namespace_record(operation_id);
        let mut request = client.persist_intent(record).expect("namespace admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("namespace result")
            .expect("namespace worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("namespace acknowledgement")
                .operation_id(),
            operation_id
        );

        let mut lookup = client
            .get(operation_id)
            .expect("namespace lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("namespace lookup result")
            .expect("namespace lookup worker result")
            .expect("persisted namespace");
        assert_eq!(persisted.schema_version, SCHEMA_V8);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Namespace(namespace_args))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("namespace universal intent")
                .command_kind(),
            UniversalCommandKind::Rename
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v9_render_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v9-render");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, render_args) = v9_duplicate_double_record(operation_id);
        let mut request = client.persist_intent(record).expect("render admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("render result")
            .expect("render worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("render acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client.get(operation_id).expect("render lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("render lookup result")
            .expect("render lookup worker result")
            .expect("persisted render");
        assert_eq!(persisted.schema_version, SCHEMA_V9);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Render(
                super::super::render_commands::RenderCommandArgs::DuplicateDouble(render_args)
            ))
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v10_removal_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v10-removal");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, removal_args) = v10_trash_record(operation_id);
        let mut request = client.persist_intent(record).expect("removal admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("removal result")
            .expect("removal worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("removal acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client.get(operation_id).expect("removal lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("removal lookup result")
            .expect("removal lookup worker result")
            .expect("persisted removal");
        assert_eq!(persisted.schema_version, SCHEMA_V10);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(
                removal_args,
            )))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("removal universal intent")
                .command_kind(),
            UniversalCommandKind::Trash
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v11_retained_resolution_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v11-retained-resolution");
        let client = owner.client();
        let operation_id = OperationId::new();
        let record = v11_retained_restore_record(operation_id);
        let mut request = client
            .persist_intent(record)
            .expect("retained resolution admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("retained resolution result")
            .expect("retained resolution worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("retained resolution acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client
            .get(operation_id)
            .expect("retained resolution lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("retained resolution lookup result")
            .expect("retained resolution lookup worker result")
            .expect("persisted retained resolution");
        assert_eq!(persisted.schema_version, SCHEMA_V11);
        assert!(matches!(
            persisted.universal_command_args(),
            Some(UniversalCommandArgs::RetainedResolution(_))
        ));
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("retained resolution universal intent")
                .command_kind(),
            UniversalCommandKind::RestoreRetained
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v12_selection_normalize_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v12-selection-normalize");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, selection_args) = v12_selection_normalize_record(operation_id);
        let mut request = client
            .persist_intent(record)
            .expect("selection Normalize admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("selection Normalize result")
            .expect("selection Normalize worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("selection Normalize acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client
            .get(operation_id)
            .expect("selection Normalize lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("selection Normalize lookup result")
            .expect("selection Normalize lookup worker result")
            .expect("persisted selection Normalize");
        assert_eq!(persisted.schema_version, SCHEMA_V12);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionNormalize(selection_args))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("selection Normalize universal intent")
                .command_kind(),
            UniversalCommandKind::DestructiveEdit
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v13_extract_and_trim_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v13-selection-output");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, command_args) = v13_extract_and_trim_record(operation_id);
        let mut request = client
            .persist_intent(record)
            .expect("selection output admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("selection output result")
            .expect("selection output worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("selection output acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client
            .get(operation_id)
            .expect("selection output lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("selection output lookup result")
            .expect("selection output lookup worker result")
            .expect("persisted selection output");
        assert_eq!(persisted.schema_version, SCHEMA_V13);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionOutput(
                SelectionOutputCommandArgs::ExtractAndTrim(command_args)
            ))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("selection output universal intent")
                .command_kind(),
            UniversalCommandKind::DestructiveEdit
        );
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v14_history_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v14-history");
        let client = owner.client();
        let operation_id = OperationId::new();
        let (record, command_args) = v14_history_record(operation_id);
        let mut request = client.persist_intent(record).expect("history admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("history result")
            .expect("history worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("history acknowledgement")
                .operation_id(),
            operation_id
        );

        let mut lookup = client.get(operation_id).expect("history lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("history lookup result")
            .expect("history lookup worker result")
            .expect("persisted history");
        assert_eq!(persisted.schema_version, SCHEMA_V14);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::History(command_args))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("history universal intent")
                .command_kind(),
            UniversalCommandKind::Undo
        );
        assert_eq!(persisted.intent.kind, OperationKind::FileHistory);
        assert!(owner.shutdown());
    }

    #[test]
    fn owner_persists_v15_external_reconciliation_through_generic_encoded_command() {
        let (_fixture, mut owner) = open_owner("journal-owner-v15-external");
        let client = owner.client();
        let operation_id = OperationId::new();
        let record = v15_external_record(operation_id);
        let mut request = client.persist_intent(record).expect("external admission");
        let outcome = request
            .recv_timeout(Duration::from_secs(2))
            .expect("external result")
            .expect("external worker result");
        assert_eq!(
            outcome
                .receipt()
                .expect("external acknowledgement")
                .operation_id(),
            operation_id
        );
        let mut lookup = client.get(operation_id).expect("external lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("external lookup result")
            .expect("external lookup worker result")
            .expect("persisted external");
        assert_eq!(persisted.schema_version, SCHEMA_V15);
        let Some(UniversalCommandArgs::ExternalReconciliation(args)) =
            persisted.universal_command_args()
        else {
            panic!("V15 external arguments");
        };
        assert_eq!(args.evidence_batch_id().as_uuid().as_u128(), 44);
        assert_eq!(args.raw().observations().len(), 1);
        assert_eq!(persisted.intent.actor, OperationActor::ExternalFs);
        assert_eq!(persisted.intent.kind, OperationKind::ExternalReconciliation);
        assert!(owner.shutdown());
    }

    #[test]
    fn result_capacity_is_held_until_consumed_and_refunded_on_drop() {
        let (_fixture, mut owner) = open_owner("journal-owner-results");
        let client = owner.client();
        let mut requests = Vec::new();
        for _ in 0..ORDINARY_RESULT_CAPACITY {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match client.get(OperationId::new()) {
                    Ok(request) => {
                        requests.push(request);
                        break;
                    }
                    Err(OperationJournalOwnerQueueError::Full)
                        if std::time::Instant::now() < deadline =>
                    {
                        thread::yield_now()
                    }
                    Err(error) => panic!("result admission: {error:?}"),
                }
            }
        }
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        drop(requests.pop().expect("held result"));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let replacement = loop {
            match client.get(OperationId::new()) {
                Ok(request) => break request,
                Err(OperationJournalOwnerQueueError::Full)
                    if std::time::Instant::now() < deadline =>
                {
                    thread::yield_now()
                }
                Err(error) => panic!("dropped result did not refund admission: {error:?}"),
            }
        };
        drop(replacement);
        drop(requests);
        assert!(owner.shutdown());
    }

    #[test]
    fn command_queue_is_bounded_and_queue_failure_refunds_credits() {
        let fixture = fixture("journal-owner-queue");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let first = client.get(OperationId::new()).expect("first admission");
        ready.wait();
        for _ in 0..ORDINARY_COMMAND_CAPACITY {
            let request = client.get(OperationId::new()).expect("queue admission");
            drop(request);
        }
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Full)
        ));
        release.wait();
        drop(first);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut request = loop {
            match client.get(OperationId::new()) {
                Ok(request) => break request,
                Err(OperationJournalOwnerQueueError::Full)
                    if std::time::Instant::now() < deadline =>
                {
                    thread::yield_now()
                }
                Err(error) => panic!("refunded admission: {error:?}"),
            }
        };
        let _ = request.recv_timeout(Duration::from_secs(2));
        assert!(owner.shutdown());
    }

    #[test]
    fn dropped_result_still_commits_and_shutdown_releases_profile_ownership() {
        let Fixture {
            owner: profile_owner,
            _profile,
            _config,
            _base,
            _runtime,
        } = fixture("journal-owner-drop-result");
        let store = OperationJournalStore::open(&profile_owner).expect("journal store");
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let operation_id = OperationId::new();
        let request = client
            .persist_intent(intent_record(operation_id))
            .expect("admission");
        ready.wait();
        drop(request);
        release.wait();
        assert!(owner.shutdown());
        drop(profile_owner);
        let replacement = WritableProfileGuard::acquire_current().expect("ownership released");
        let store = OperationJournalStore::open(&replacement).expect("reopen journal");
        assert_eq!(
            store
                .get(operation_id)
                .expect("read committed row")
                .map(|record| record.operation_id),
            Some(operation_id)
        );
    }

    #[test]
    fn worker_panic_stays_unavailable_through_shutdown() {
        let fixture = fixture("journal-owner-panic");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_worker_panic_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let mut request = client.get(OperationId::new()).expect("admission");
        ready.wait();
        let (shutdown_ready, shutdown_release) = owner.shared.set_shutdown_cas_gate_for_test();
        let shutdown = thread::spawn(move || owner.shutdown());
        shutdown_ready.wait();
        release.wait();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(
                client.status(),
                OperationJournalOwnerStatus::Unavailable { .. }
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not report unavailable"
            );
            thread::yield_now();
        }
        shutdown_release.wait();
        assert!(!shutdown.join().expect("shutdown thread"));
        assert!(matches!(
            client.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        assert!(matches!(
            request.recv_timeout(Duration::from_secs(2)),
            Err(OperationJournalRequestError::Disconnected)
        ));
    }

    #[test]
    fn fifo_v5_retry_and_cancel_are_serialized_by_one_worker() {
        let (_fixture, mut owner) = open_owner("journal-owner-fifo");
        let client = owner.client();
        let first_id = OperationId::new();
        let second_id = OperationId::new();
        let (first_record, expected_args) = v5_copy_record(first_id);
        let mut first = client
            .persist_intent(first_record)
            .expect("first admission");
        let mut second = client
            .persist_intent(intent_record(second_id))
            .expect("second admission");
        let first_receipt = first
            .recv_timeout(Duration::from_secs(2))
            .expect("first result")
            .expect("first worker")
            .receipt()
            .expect("first ack");
        let second_receipt = second
            .recv_timeout(Duration::from_secs(2))
            .expect("second result")
            .expect("second worker")
            .receipt()
            .expect("second ack");
        assert_eq!(first_receipt.sequence(), 1);
        assert_eq!(second_receipt.sequence(), 2);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("owner-retry").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "owner_busy",
            Some(String::from("owner retry")),
        )
        .expect("retry observation");
        let mut retry_request = client
            .record_retry_pending(first_id, first_receipt.sequence(), retry.clone())
            .expect("cancel admission");
        let retry_receipt = retry_request
            .recv_timeout(Duration::from_secs(2))
            .expect("retry result")
            .expect("retry worker");
        assert_eq!(retry_receipt.operation_id(), first_id);
        assert_eq!(retry_receipt.sequence(), 3);

        let cancellation = CancellationRequestObservation::new(56).expect("cancel observation");
        let mut cancel_request = client
            .request_cancel_before_publish(first_id, retry_receipt.sequence(), cancellation)
            .expect("retry admission");
        let cancel_receipt = cancel_request
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel result")
            .expect("cancel worker");
        assert_eq!(cancel_receipt.operation_id(), first_id);
        assert_eq!(cancel_receipt.sequence(), 4);

        let mut lookup = client.get(first_id).expect("lookup admission");
        let persisted = lookup
            .recv_timeout(Duration::from_secs(2))
            .expect("lookup result")
            .expect("lookup worker")
            .expect("persisted V5 row");
        assert_eq!(persisted.schema_version, SCHEMA_V5);
        assert_eq!(persisted.operation_id, first_id);
        assert_eq!(
            persisted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(persisted.command_args(), Some(&expected_args));
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        assert_eq!(persisted.created_unix_ms, 50);
        assert_eq!(persisted.updated_unix_ms, cancellation.requested_unix_ms());

        let cursor = RecoveryCursor::new(first_receipt.sequence()).expect("initial cursor");
        let mut page_request = client
            .scan_recovery_page(Some(cursor))
            .expect("recovery page admission");
        let page = page_request
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery page result")
            .expect("recovery page worker");
        assert!(page.entries().iter().any(|entry| entry.sequence() == 2));
        let updated_entry = page
            .entries()
            .iter()
            .find(|entry| match entry.outcome() {
                RecoveryRowOutcome::Consistent(record) => record.operation_id == first_id,
                RecoveryRowOutcome::Retained(_) => false,
            })
            .expect("updated first operation after old cursor");
        assert_eq!(updated_entry.sequence(), cancel_receipt.sequence());
        match updated_entry.outcome() {
            RecoveryRowOutcome::Consistent(record) => {
                assert_eq!(record.command_args(), Some(&expected_args));
                assert_eq!(record.retry_observation(), Some(&retry));
                assert_eq!(record.cancellation_request(), Some(&cancellation));
                assert_eq!(
                    record.disposition,
                    OperationDisposition::CancelRequestedBeforePublish
                );
            }
            RecoveryRowOutcome::Retained(_) => unreachable!("updated V5 record retained"),
        }
        assert!(owner.shutdown());
    }

    #[test]
    fn retained_client_rejects_after_shutdown_and_queued_work_drains() {
        let fixture = fixture("journal-owner-shutdown");
        let store = OperationJournalStore::open(&fixture.owner).expect("journal store");
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let operation_id = OperationId::new();
        let mut request = client
            .persist_intent(intent_record(operation_id))
            .expect("admission");
        ready.wait();
        let join = thread::spawn(move || {
            release.wait();
        });
        assert!(owner.shutdown());
        join.join().expect("release gate");
        assert!(request.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
        drop(fixture);
    }

    #[test]
    fn oversized_input_is_rejected_before_queue_admission() {
        let (_fixture, mut owner) = open_owner("journal-owner-bounds");
        let client = owner.client();
        let mut record = intent_record(OperationId::new());
        record.payload = serde_json::Value::String("x".repeat(MAX_RECORD_BYTES));
        assert!(matches!(
            client.persist_intent(record),
            Err(OperationJournalOwnerQueueError::RecordTooLarge { .. })
                | Err(OperationJournalOwnerQueueError::InvalidRecord { .. })
        ));
        assert!(owner.shutdown());
    }

    #[test]
    fn oversized_decoded_retry_observation_is_rejected_before_queue_credits() {
        let (_fixture, mut owner) = open_owner("journal-owner-retry-bounds");
        let client = owner.client();
        let observation: RetryObservation = serde_json::from_value(serde_json::json!({
            "attempt": 1,
            "requested_unix_ms": 1,
            "next_retry_unix_ms": 2,
            "lease_id": "retry-lease",
            "lease_expiry_unix_ms": 3,
            "error_class": "TransientContention",
            "diagnostic_code": "x".repeat(MAX_DIAGNOSTIC_CODE_BYTES + 1),
            "redacted_context": null
        }))
        .expect("decode oversized observation fixture");
        assert!(matches!(
            client.record_retry_pending(OperationId::new(), 1, observation),
            Err(OperationJournalOwnerQueueError::InvalidInput { .. })
        ));

        let mut lookup = client
            .get(OperationId::new())
            .expect("input credit preserved");
        assert!(
            lookup
                .recv_timeout(Duration::from_secs(2))
                .expect("lookup result")
                .expect("lookup worker result")
                .is_none()
        );
        assert!(owner.shutdown());
    }
}
