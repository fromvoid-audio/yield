//! Empty, pre-intent reservations on the existing global worker. No journal authority.
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::sample_sources::db::sqlite_identity::{
    OpenedDatabaseFile, open_existing_database_file, validate_connection_identity,
    validate_database_file_identity,
};

/// Maximum complete preparation handshake encoding, across all participants.
pub const GLOBAL_PREPARATION_MAX_BYTES: usize = 64 * 1024;
/// Maximum affected source and global databases combined.
pub const GLOBAL_PREPARATION_MAX_DATABASES: usize = 64;
/// Cooperative pre-intent deadline, including time spent in the ordinary queue.
pub const GLOBAL_PREPARATION_MAX_DURATION: Duration = Duration::from_secs(2);

/// Bounded local preparation failure. No error grants a durable operation or capacity.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GlobalPreparationError {
    /// The full handshake exceeds a protocol bound.
    #[error("preparation input exceeds fixed bounds")]
    InputBound,
    /// Queue wait/acquisition consumed the original deadline.
    #[error("preparation deadline elapsed")]
    Deadline,
    /// The request belongs to another profile acquisition or retired worker.
    #[error("preparation owner changed")]
    StaleOwner,
    /// A request or delivered reservation was canceled before any intent.
    #[error("preparation canceled")]
    Canceled,
    /// Shutdown began; no preparation may survive it.
    #[error("preparation owner is closing")]
    Closing,
    /// The retained root/main file/native SQLite binding cannot be proven.
    #[error("preparation database binding unavailable")]
    DatabaseBinding,
    /// Exactly one nonblocking acquisition failed; preserve SQLite's extended code.
    #[error("preparation SQLite acquisition failed (code {sqlite_code})")]
    Acquisition {
        /// Actual SQLite extended code, never an attempt count or success receipt.
        sqlite_code: i32,
    },
    /// Actual retained VFS measurement rejects this empty reservation.
    #[error(transparent)]
    Wal(#[from] crate::WalAdmissionError),
    /// Timeout restoration or empty rollback failed; the connection must be retired.
    #[error("preparation connection requires retirement")]
    ConnectionRetired,
}

/// Observation of one actual worker registration within a profile acquisition.
/// Private construction prevents caller substitution; copying it conveys no authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalPreparationOwner {
    acquisition: ProfileAcquisitionId,
    registration: uuid::Uuid,
}
impl GlobalPreparationOwner {
    pub(super) fn new(acquisition: ProfileAcquisitionId) -> Self {
        Self {
            acquisition,
            registration: uuid::Uuid::new_v4(),
        }
    }
    /// Profile acquisition observed by this worker.
    pub fn acquisition(&self) -> &ProfileAcquisitionId {
        &self.acquisition
    }
    /// Actual worker registration; restarting within the same acquisition changes it.
    pub fn registration(&self) -> uuid::Uuid {
        self.registration
    }
}

/// Immutable bounded correlation. The caller's digest and inventory sizes are not authority.
/// The journal coordinator must independently validate full inventory and canonical encoding.
#[derive(Debug)]
pub struct GlobalPreparationRequest {
    owner: GlobalPreparationOwner,
    command_digest: [u8; 32],
    deadline: Instant,
}
impl GlobalPreparationRequest {
    /// Use the coordinator's original deadline; never restart it on delayed submission.
    pub fn new(
        owner: GlobalPreparationOwner,
        command_digest: [u8; 32],
        deadline: Instant,
        affected_databases: usize,
        handshake_bytes: usize,
    ) -> Result<Self, GlobalPreparationError> {
        if !(1..=GLOBAL_PREPARATION_MAX_DATABASES).contains(&affected_databases)
            || !(32..=GLOBAL_PREPARATION_MAX_BYTES).contains(&handshake_bytes)
        {
            return Err(GlobalPreparationError::InputBound);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(GlobalPreparationError::Deadline)?;
        if remaining > GLOBAL_PREPARATION_MAX_DURATION {
            return Err(GlobalPreparationError::InputBound);
        }
        Ok(Self {
            owner,
            command_digest,
            deadline,
        })
    }
}

/// Copyable diagnostics from the actual retained worker, not a reusable grant or latch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalPreparationObservation {
    owner: GlobalPreparationOwner,
    database_identity: String,
    command_digest: [u8; 32],
    nonce: uuid::Uuid,
    wal_bytes: u64,
}
impl GlobalPreparationObservation {
    /// Profile acquisition actually validated by the issuing worker.
    pub fn acquisition(&self) -> &ProfileAcquisitionId {
        self.owner.acquisition()
    }
    /// Exact worker registration behind this diagnostic.
    pub fn owner(&self) -> &GlobalPreparationOwner {
        &self.owner
    }
    /// Descriptor-derived physical identity for coordinator alias/conflict checks.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }
    /// Immutable command correlation; not proof of a validated journal command.
    pub fn command_digest(&self) -> &[u8; 32] {
        &self.command_digest
    }
    /// Owner-issued, non-durable correlation, never an OperationId.
    pub fn nonce(&self) -> uuid::Uuid {
        self.nonce
    }
    /// Actual retained WAL size under the empty writer lock.
    pub fn wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Diagnostic request for passive catch-up after release, not a checkpoint request.
    pub fn needs_catch_up(&self) -> bool {
        self.wal_bytes >= crate::WAL_SOFT_WATERMARK_BYTES
    }
}

/// A pending delivery. Dropping it cancels queued/held preparation without losing the owner.
pub struct GlobalPreparationPending {
    ready: Receiver<Result<GlobalPreparationReservation, GlobalPreparationError>>,
    canceled: Arc<AtomicBool>,
    armed: bool,
}
impl GlobalPreparationPending {
    /// Poll without blocking the caller; successful delivery transfers cancellation ownership.
    pub fn try_recv(
        &mut self,
    ) -> Result<Result<GlobalPreparationReservation, GlobalPreparationError>, mpsc::TryRecvError>
    {
        let result = self.ready.try_recv()?;
        self.armed = false;
        Ok(result)
    }
    /// Bounded non-UI/test wait. A wait timeout does not extend preparation's deadline.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Result<GlobalPreparationReservation, GlobalPreparationError>, mpsc::RecvTimeoutError>
    {
        let result = self.ready.recv_timeout(timeout)?;
        self.armed = false;
        Ok(result)
    }
}
impl Drop for GlobalPreparationPending {
    fn drop(&mut self) {
        if self.armed {
            self.canceled.store(true, Ordering::Release);
        }
    }
}

enum Control {
    Abort,
}
/// Live, non-cloneable cancellation handle. It cannot authorize intent or SQL effects.
/// Dropping it requests rollback; ordinary reuse occurs only after confirmed rollback.
pub struct GlobalPreparationReservation {
    observation: GlobalPreparationObservation,
    canceled: Arc<AtomicBool>,
    control: SyncSender<Control>,
    released: Option<Receiver<Result<(), GlobalPreparationError>>>,
}
impl GlobalPreparationReservation {
    /// Borrow authority-free diagnostics while the owner may still hold the reservation.
    pub fn observation(&self) -> &GlobalPreparationObservation {
        &self.observation
    }
    /// Request abort and obtain confirmation of the empty rollback. A lost response is not
    /// proof of release; the worker still owns reconciliation/connection retirement.
    pub fn abort(mut self) -> Receiver<Result<(), GlobalPreparationError>> {
        self.canceled.store(true, Ordering::Release);
        let _ = self.control.try_send(Control::Abort);
        self.released.take().expect("single release receiver")
    }
}
impl Drop for GlobalPreparationReservation {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Release);
        let _ = self.control.try_send(Control::Abort);
    }
}

pub(super) struct Command {
    request: GlobalPreparationRequest,
    canceled: Arc<AtomicBool>,
    ready: SyncSender<Result<GlobalPreparationReservation, GlobalPreparationError>>,
}
impl Command {
    pub(super) fn reject(self) {
        let _ = self.ready.try_send(Err(GlobalPreparationError::StaleOwner));
    }
}
impl GlobalLibraryWriter {
    /// Capture this actual worker registration before forming a delayed preparation.
    pub fn preparation_owner(&self) -> GlobalPreparationOwner {
        self.preparation_owner.clone()
    }
    /// Queue a bounded empty preparation on this retained owner. Queue acceptance is not
    /// writer acquisition, durable intent, a decision latch or ordinary action completion.
    pub fn submit_preparation(
        &self,
        request: GlobalPreparationRequest,
    ) -> Result<GlobalPreparationPending, GlobalLibraryWriterQueueError> {
        let (ready, receiver) = mpsc::sync_channel(1);
        let canceled = Arc::new(AtomicBool::new(false));
        self.send(WriterCommand::Prepare(Command {
            request,
            canceled: Arc::clone(&canceled),
            ready,
        }))?;
        Ok(GlobalPreparationPending {
            ready: receiver,
            canceled,
            armed: true,
        })
    }
}

fn validate(
    database: &LibraryDatabase,
    guard: &WritableProfileGuard,
    file: &OpenedDatabaseFile,
) -> Result<(), GlobalPreparationError> {
    database
        .validate_profile_guard(guard)
        .map_err(|_| GlobalPreparationError::StaleOwner)?;
    let root = database
        .preparation_root()
        .ok_or(GlobalPreparationError::DatabaseBinding)?;
    let name = Path::new(super::super::LIBRARY_DB_FILE_NAME);
    let path = root.path().join(name);
    validate_database_file_identity(root, name, &path, &file.identity)
        .and_then(|_| validate_connection_identity(&database.connection, &path, &file.identity))
        .map_err(|_| GlobalPreparationError::DatabaseBinding)
}
fn canceled(
    command: &Command,
    lifecycle: &std::sync::atomic::AtomicU8,
) -> Result<(), GlobalPreparationError> {
    if WriterLifecycle::from_u8(lifecycle.load(Ordering::Acquire)) != WriterLifecycle::Available {
        return Err(GlobalPreparationError::Closing);
    }
    if command.canceled.load(Ordering::Acquire) {
        return Err(GlobalPreparationError::Canceled);
    }
    if Instant::now() >= command.request.deadline {
        return Err(GlobalPreparationError::Deadline);
    }
    Ok(())
}
fn sql_error(error: rusqlite::Error) -> GlobalPreparationError {
    GlobalPreparationError::Acquisition {
        sqlite_code: match error {
            rusqlite::Error::SqliteFailure(error, _) => error.extended_code,
            _ => rusqlite::ffi::SQLITE_ERROR,
        },
    }
}

/// Returns false only when the connection must close before any further dispatch.
pub(super) fn run(
    command: Command,
    database: &LibraryDatabase,
    guard: &WritableProfileGuard,
    lifecycle: &std::sync::atomic::AtomicU8,
    registration: uuid::Uuid,
) -> bool {
    let mut began = false;
    let preparation = (|| {
        canceled(&command, lifecycle)?;
        if command.request.owner.acquisition.as_str() != guard.owner_identity()
            || command.request.owner.registration != registration
        {
            return Err(GlobalPreparationError::StaleOwner);
        }
        database
            .validate_profile_guard(guard)
            .map_err(|_| GlobalPreparationError::StaleOwner)?;
        let root = database
            .preparation_root()
            .ok_or(GlobalPreparationError::DatabaseBinding)?;
        let name = Path::new(super::super::LIBRARY_DB_FILE_NAME);
        let file = open_existing_database_file(root, name, &root.path().join(name))
            .map_err(|_| GlobalPreparationError::DatabaseBinding)?;
        if file.identity.len() > 256 {
            return Err(GlobalPreparationError::InputBound);
        }
        validate(database, guard, &file)?;
        if !database.connection.is_autocommit() {
            return Err(GlobalPreparationError::ConnectionRetired);
        }
        let timeout: u64 = database
            .connection
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .map_err(sql_error)?;
        let begin = database
            .connection
            .busy_timeout(Duration::ZERO)
            .map_err(sql_error)
            .and_then(|_| {
                canceled(&command, lifecycle)?;
                database
                    .connection
                    .execute_batch("BEGIN IMMEDIATE")
                    .map_err(sql_error)
            });
        began = !database.connection.is_autocommit();
        // Restore the exact ordinary policy even on acquisition failure. No retry/sleep here.
        database
            .connection
            .busy_timeout(Duration::from_millis(timeout))
            .map_err(|_| GlobalPreparationError::ConnectionRetired)?;
        begin?;
        validate(database, guard, &file)?;
        canceled(&command, lifecycle)?;
        let wal_bytes =
            crate::sqlite_wal::enforce_wal_admission(&database.connection)?.observed_bytes();
        canceled(&command, lifecycle)?;
        let (control, controls) = mpsc::sync_channel(1);
        let (released, release_rx) = mpsc::sync_channel(1);
        let reservation = GlobalPreparationReservation {
            observation: GlobalPreparationObservation {
                owner: command.request.owner.clone(),
                database_identity: file.identity.clone(),
                command_digest: command.request.command_digest,
                nonce: uuid::Uuid::new_v4(),
                wal_bytes,
            },
            canceled: Arc::clone(&command.canceled),
            control,
            released: Some(release_rx),
        };
        if command.ready.try_send(Ok(reservation)).is_err() {
            command.canceled.store(true, Ordering::Release);
        }
        // No SQL/row query/checkpoint/renewal/ordinary dispatch while held. The main-file FD
        // remains alive, and actual owner/path/native binding is rechecked cooperatively.
        let outcome = loop {
            if let Err(error) = canceled(&command, lifecycle) {
                break error;
            }
            if let Err(error) = validate(database, guard, &file) {
                break error;
            }
            let wait = OWNER_POLL_INTERVAL.min(
                command
                    .request
                    .deadline
                    .saturating_duration_since(Instant::now()),
            );
            match controls.recv_timeout(wait) {
                Ok(Control::Abort) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break GlobalPreparationError::Canceled;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        let rollback = database.connection.execute_batch("ROLLBACK");
        began = !database.connection.is_autocommit();
        let reusable =
            rollback.is_ok() && !began && outcome != GlobalPreparationError::DatabaseBinding;
        let result = if rollback.is_err() || began {
            Err(GlobalPreparationError::ConnectionRetired)
        } else if outcome == GlobalPreparationError::Canceled {
            Ok(())
        } else {
            Err(outcome)
        };
        let _ = released.try_send(result);
        Ok(reusable)
    })();
    match preparation {
        Ok(reusable) => reusable,
        Err(error) => {
            let rollback_ok = !began
                || (database.connection.execute_batch("ROLLBACK").is_ok()
                    && database.connection.is_autocommit());
            let reusable = rollback_ok
                && error != GlobalPreparationError::ConnectionRetired
                && error != GlobalPreparationError::DatabaseBinding;
            let _ = command.ready.try_send(Err(if rollback_ok {
                error
            } else {
                GlobalPreparationError::ConnectionRetired
            }));
            reusable
        }
    }
}

#[cfg(test)]
mod tests;
