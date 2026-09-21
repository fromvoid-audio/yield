//! Asynchronous ownership for the profile-local operation journal.
//!
//! The owner moves an already-open [`OperationJournalStore`] onto one worker thread. Its client
//! handles contain only bounded command and result state; they do not retain a profile guard or a
//! SQLite connection. A successful queue submission means that storage work was admitted to this
//! worker. It does not grant operation admission, filesystem authority, capacity authority, or a
//! tentative persistence receipt.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

use super::capacity_provider;
use super::codec::{CappedWriter, DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::coordinator::{RetryObservation, UniversalCommandArgs};
use super::full_progress_codec::{
    FullProgressCodecError, StoredFullProgressRecord, decode as decode_full_progress,
    encode as encode_full_progress,
};
use super::model::{CancellationRequestObservation, MAX_RECORD_BYTES, OperationId};
use super::participants::ParticipantSet;
use super::store::recovery::{
    CoordinatorRecoveryPage, RecoveryCursor, RecoveryPage, RecoveryScanError,
};
use super::store::{
    FullProgressWriteOutcome, JournalCancelRequestReceipt, JournalInsertOutcome,
    JournalRetryReceipt, OperationJournalStore, OperationJournalStoreError,
};
use super::work_plan::derive_work_plan;

const CONTROL_INPUT_BYTES: usize = 1;

/// Maximum number of journal commands waiting for the storage owner.
pub const OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY: usize = 32;

/// Maximum number of results that may remain outstanding across all journal clients.
pub const OPERATION_JOURNAL_OWNER_RESULT_CAPACITY: usize = 64;

/// Maximum encoded record bytes reserved by queued journal commands.
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
    /// The fixed-capacity command queue is full.
    #[error("operation journal owner command queue is full")]
    Full,
    /// All bounded result handles are already outstanding.
    #[error("operation journal owner result capacity is full")]
    ResultsFull,
    /// The bounded input-byte budget is currently exhausted.
    #[error(
        "operation journal owner queued input exceeds its {limit}-byte budget (requested {requested} bytes)"
    )]
    InputBytesFull {
        /// Total encoded command input bytes available to the queue.
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

/// Failure returned by an admitted journal command.
#[derive(Debug, Error)]
pub enum OperationJournalOwnerError {
    /// The underlying journal store rejected or could not complete the command.
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
    /// The bounded record bytes could not be decoded on the owner thread.
    #[error(transparent)]
    Codec(#[from] DecodeError),
    /// A recovery page could not be read or retained its audit-required state.
    #[error(transparent)]
    Recovery(#[from] RecoveryScanError),
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
    receiver: Receiver<Result<T, OperationJournalOwnerError>>,
    result_permit: Option<ResultPermit>,
}

impl<T> OperationJournalRequest<T> {
    /// Poll the result without waiting for the worker.
    pub fn try_result(
        &mut self,
    ) -> Result<Result<T, OperationJournalOwnerError>, OperationJournalRequestError> {
        match self.receiver.try_recv() {
            Ok(result) => {
                self.result_permit.take();
                Ok(result)
            }
            Err(TryRecvError::Empty) => Err(OperationJournalRequestError::Empty),
            Err(TryRecvError::Disconnected) => {
                self.result_permit.take();
                Err(OperationJournalRequestError::Disconnected)
            }
        }
    }

    /// Wait up to `timeout` for the result without changing command admission semantics.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Result<T, OperationJournalOwnerError>, OperationJournalRequestError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => {
                self.result_permit.take();
                Ok(result)
            }
            Err(RecvTimeoutError::Timeout) => Err(OperationJournalRequestError::Empty),
            Err(RecvTimeoutError::Disconnected) => {
                self.result_permit.take();
                Err(OperationJournalRequestError::Disconnected)
            }
        }
    }
}

impl<T> Drop for OperationJournalRequest<T> {
    fn drop(&mut self) {
        self.result_permit.take();
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

/// The single asynchronous owner of one already-open operation journal.
///
/// The supplied store must have passed the caller's recovery gate before handoff. The owner takes
/// the store by value and releases it on its worker thread after accepted commands have drained.
pub struct OperationJournalOwner {
    shared: Arc<OwnerShared>,
    worker: Option<JoinHandle<()>>,
}

impl OperationJournalOwner {
    /// Start a worker by consuming an already-open journal store.
    pub fn start(store: OperationJournalStore) -> Result<Self, OperationJournalOwnerStartError> {
        Self::start_inner(
            store,
            #[cfg(test)]
            None,
            #[cfg(test)]
            false,
        )
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
        )
        .expect("operation journal owner start");
        (owner, ready, release)
    }

    #[cfg(test)]
    fn start_with_worker_panic_for_test(
        store: OperationJournalStore,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let owner = Self::start_inner(
            store,
            Some((Arc::clone(&ready), Arc::clone(&release))),
            true,
        )
        .expect("operation journal owner start");
        (owner, ready, release)
    }

    fn start_inner(
        store: OperationJournalStore,
        #[cfg(test)] command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        #[cfg(test)] panic_on_command: bool,
    ) -> Result<Self, OperationJournalOwnerStartError> {
        let (command_tx, command_rx) = mpsc::sync_channel(OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY);
        let shared = Arc::new(OwnerShared::new(command_tx));
        let panic_shared = Arc::clone(&shared);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-operation-journal-owner"))
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_owner(
                        store,
                        command_rx,
                        worker_shared,
                        #[cfg(test)]
                        command_gate,
                        #[cfg(test)]
                        panic_on_command,
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

    /// Stop new admission, drain accepted commands, release the journal, and join the worker.
    ///
    /// Returns `true` only when the worker drained and stopped cleanly. A worker failure returns
    /// `false` and remains visible as [`OperationJournalOwnerStatus::Unavailable`] to retained
    /// clients after the worker resources have been released. Calling shutdown again after a
    /// clean close also returns `false`.
    pub fn shutdown(&mut self) -> bool {
        let previous = {
            let _admission = self
                .shared
                .admission_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = loop {
                let current = self.shared.lifecycle.load(Ordering::Acquire);
                let current_state = OwnerLifecycle::from_u8(current);
                if matches!(
                    current_state,
                    OwnerLifecycle::Unavailable | OwnerLifecycle::Closed
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
            };
            self.shared
                .commands
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            previous
        };
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
    /// Return the current worker lifecycle without waiting for storage I/O.
    pub fn status(&self) -> OperationJournalOwnerStatus {
        self.shared.status()
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
        self.submit(encoded.len(), move |input, result| JournalCommand::Cancel {
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
        self.submit(encoded.len(), move |input, result| JournalCommand::Retry {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
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

    fn submit<T, F>(
        &self,
        input_bytes: usize,
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
        let input = self.shared.reserve_input(input_bytes)?;
        let result_permit = self.shared.reserve_result()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let command = build(input, result_tx);
        match sender.try_send(command) {
            Ok(()) => Ok(OperationJournalRequest {
                receiver: result_rx,
                result_permit: Some(result_permit),
            }),
            Err(TrySendError::Full(_command)) => {
                drop(result_permit);
                Err(OperationJournalOwnerQueueError::Full)
            }
            Err(TrySendError::Disconnected(_command)) => {
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
    commands: Mutex<Option<SyncSender<JournalCommand>>>,
    admission_gate: Mutex<()>,
    lifecycle: AtomicU8,
    unavailable: Mutex<Option<String>>,
    input_bytes: Mutex<usize>,
    results: Mutex<usize>,
    #[cfg(test)]
    shutdown_cas_gate: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}

impl OwnerShared {
    fn new(commands: SyncSender<JournalCommand>) -> Self {
        Self {
            commands: Mutex::new(Some(commands)),
            admission_gate: Mutex::new(()),
            lifecycle: AtomicU8::new(OwnerLifecycle::Initializing as u8),
            unavailable: Mutex::new(None),
            input_bytes: Mutex::new(OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES),
            results: Mutex::new(OPERATION_JOURNAL_OWNER_RESULT_CAPACITY),
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
        requested: usize,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        let mut remaining = self
            .input_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if requested > *remaining {
            return Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES,
                requested,
            });
        }
        *remaining -= requested;
        Ok(InputPermit {
            shared: Arc::clone(self),
            bytes: requested,
        })
    }

    fn reserve_result(self: &Arc<Self>) -> Result<ResultPermit, OperationJournalOwnerQueueError> {
        let mut remaining = self
            .results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *remaining == 0 {
            return Err(OperationJournalOwnerQueueError::ResultsFull);
        }
        *remaining -= 1;
        Ok(ResultPermit {
            shared: Arc::clone(self),
        })
    }

    fn release_input(&self, bytes: usize) {
        let mut remaining = self
            .input_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining
            .checked_add(bytes)
            .expect("operation journal input credit overflow");
        assert!(*remaining <= OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES);
    }

    fn release_result(&self) {
        let mut remaining = self
            .results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining
            .checked_add(1)
            .expect("operation journal result credit overflow");
        assert!(*remaining <= OPERATION_JOURNAL_OWNER_RESULT_CAPACITY);
    }

    fn mark_unavailable(&self, reason: &str) {
        *self
            .unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(String::from(reason));
        self.lifecycle
            .store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
    }
}

struct InputPermit {
    shared: Arc<OwnerShared>,
    bytes: usize,
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        self.shared.release_input(self.bytes);
    }
}

struct ResultPermit {
    shared: Arc<OwnerShared>,
}

impl Drop for ResultPermit {
    fn drop(&mut self) {
        self.shared.release_result();
    }
}

#[allow(dead_code)]
enum JournalCommand {
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
    AdmitFull {
        args: UniversalCommandArgs,
        participants: ParticipantSet,
        input: InputPermit,
        result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
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
    GetFull {
        operation_id: OperationId,
        input: InputPermit,
        result: SyncSender<Result<Option<StoredFullProgressRecord>, OperationJournalOwnerError>>,
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
    command_rx: Receiver<JournalCommand>,
    shared: Arc<OwnerShared>,
    #[cfg(test)] command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    #[cfg(test)] panic_on_command: bool,
) {
    let starting = shared.lifecycle.compare_exchange(
        OwnerLifecycle::Initializing as u8,
        OwnerLifecycle::Available as u8,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    if let Err(state) = starting {
        if OwnerLifecycle::from_u8(state) != OwnerLifecycle::Closing {
            while let Ok(command) = command_rx.try_recv() {
                reject_command(command, OperationJournalOwnerError::Closed);
            }
            drop(store);
            return;
        }
    }
    #[cfg(test)]
    let mut command_gate = command_gate;
    #[cfg(test)]
    let panic_on_command = panic_on_command;
    loop {
        let command = match command_rx.recv() {
            Ok(command) => command,
            Err(mpsc::RecvError) => break,
        };
        #[cfg(test)]
        if let Some((ready, release)) = command_gate.take() {
            ready.wait();
            release.wait();
        }
        #[cfg(test)]
        if panic_on_command {
            panic!("operation journal owner panic fixture");
        }
        let panicked = execute_command(command, &store);
        if panicked {
            shared.mark_unavailable("operation journal owner command panicked");
            while let Ok(command) = command_rx.try_recv() {
                reject_command(command, OperationJournalOwnerError::WorkerPanicked);
            }
            break;
        }
    }
    drop(store);
    if OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
        != OwnerLifecycle::Unavailable
    {
        shared
            .lifecycle
            .store(OwnerLifecycle::Closed as u8, Ordering::Release);
    }
}

fn execute_command(command: JournalCommand, store: &OperationJournalStore) -> bool {
    match command {
        JournalCommand::PersistIntent {
            encoded,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || {
                let record = decode(&encoded)?;
                Ok::<JournalInsertOutcome, DecodeError>(store.insert_intent_with_outcome(&record))
            })
        }
        JournalCommand::PersistFull {
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || {
                let record = decode_full_progress(&encoded).map_err(|error| {
                    OperationJournalOwnerError::InvalidInput {
                        reason: error.to_string(),
                    }
                })?;
                store
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
            drop(input);
            execute_store_result(result, || {
                let observation = decode_control_input(&encoded)?;
                store
                    .request_cancel_before_publish(operation_id, expected_sequence, observation)
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::Retry {
            operation_id,
            expected_sequence,
            encoded,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || {
                let observation = decode_control_input(&encoded)?;
                store
                    .record_retry_pending(operation_id, expected_sequence, observation)
                    .map_err(OperationJournalOwnerError::from)
            })
        }
        JournalCommand::AdmitFull {
            args,
            participants,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || {
                store.validate_for_admission()?;
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
        JournalCommand::Get {
            operation_id,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || store.get(operation_id))
        }
        JournalCommand::Recovery {
            cursor,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || store.scan_recovery_page(cursor))
        }
        JournalCommand::GetFull {
            operation_id,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || {
                store
                    .get_full(operation_id)
                    .map(|row| row.map(|row| row.into_record()))
            })
        }
        JournalCommand::CoordinatorRecovery {
            cursor,
            input,
            result,
        } => {
            drop(input);
            execute_store_result(result, || store.scan_coordinator_recovery_page(cursor))
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
    match command {
        JournalCommand::PersistIntent { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::PersistFull { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::Cancel { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::Retry { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::AdmitFull { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::Get { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::Recovery { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::GetFull { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::CoordinatorRecovery { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
    };
    use crate::operation_journal::full_progress_codec::StoredFullProgressRecord;
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation, EvidenceBatchId,
        ExternalCaptureBoundary, ExternalNativeString, ExternalRawEventKind,
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
        let owner = OperationJournalOwner::start(store).expect("journal owner");
        wait_until_available(&owner);
        (fixture, owner)
    }

    fn wait_until_available(owner: &OperationJournalOwner) {
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

        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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

        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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
            OperationJournalOwner::start(reopened_store).expect("reopen owner");
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
        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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
            OperationJournalOwner::start(reopened_store).expect("reopen owner");
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

        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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

        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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

        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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
        let mut owner = OperationJournalOwner::start(store).expect("journal owner");
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
                            "relative_path": ".wavecrate_delete_staging/folder/item",
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
        for _ in 0..OPERATION_JOURNAL_OWNER_RESULT_CAPACITY {
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
        for _ in 0..OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY {
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
