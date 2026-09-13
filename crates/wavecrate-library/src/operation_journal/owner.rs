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

use super::codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::coordinator::RetryObservation;
use super::model::{CancellationRequestObservation, MAX_RECORD_BYTES, OperationId};
use super::store::recovery::{RecoveryCursor, RecoveryPage, RecoveryScanError};
use super::store::{
    JournalCancelRequestReceipt, JournalInsertOutcome, JournalRetryReceipt, OperationJournalStore,
    OperationJournalStoreError,
};

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

enum JournalCommand {
    PersistIntent {
        encoded: Vec<u8>,
        input: InputPermit,
        result: SyncSender<Result<JournalInsertOutcome, OperationJournalOwnerError>>,
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
        JournalCommand::Cancel { input, result, .. } => {
            drop(input);
            let _ = result.send(Err(error));
        }
        JournalCommand::Retry { input, result, .. } => {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
    };
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        MAX_DIAGNOSTIC_CODE_BYTES, OperationActor, OperationDisposition, OperationIntent,
        OperationKind, ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor,
        ParticipantKind, ParticipantRequirement, ParticipantSet, RecoveryCursor,
        RecoveryRowOutcome, RetryErrorClass, RetryLeaseId, SCHEMA_V5, StoredRelativePath,
        StoredSourceFence, UniversalCommandKind, UniversalIntent, decode,
    };
    use crate::sample_sources::SourceId;
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
