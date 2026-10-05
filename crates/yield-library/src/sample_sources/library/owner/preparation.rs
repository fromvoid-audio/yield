//! Empty, pre-intent reservations on the existing global worker. No journal authority.
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;

pub(super) mod binding;
use crate::sample_sources::db::sqlite_identity::{
    OpenedDatabaseFile, open_existing_database_file, validate_connection_identity,
    validate_database_file_identity,
};
pub use binding::{GlobalWalObjectObservation, GlobalWalScope};
mod requester;
pub use requester::GlobalWalPreparationClient;

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
    /// An intervening mutating dispatch invalidated the observed registration-local fence.
    #[error("global preparation mutation fence changed")]
    StaleMutationFence,
    /// The checked counter cannot advance for a subsequent effect.
    #[error("global preparation mutation fence exhausted")]
    MutationFenceExhausted,
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
    /// The original empty writer transaction exited, weakened, or changed rows.
    #[error("original empty preparation transaction changed")]
    EmptyReservationChanged,
    /// Exact lifecycle schema/runtime/retained allocation observations are unavailable.
    #[error(transparent)]
    LifecycleAllocation(#[from] crate::lifecycle_allocation::LifecycleAllocationError),
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
    expected_mutation_revision: Option<u64>,
    lifecycle_allocation: bool,
    lifecycle_policy: bool,
    wal_binding: bool,
    handshake: Option<Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
}
impl GlobalPreparationRequest {
    pub(crate) fn in_handshake(
        mut self,
        budget: Arc<crate::operation_journal::wal_admission::HandshakeBudget>,
    ) -> Self {
        self.handshake = Some(budget);
        self.wal_binding = true;
        self
    }

    /// Retain the exact capped lifecycle settings policy on the original global worker.
    /// This pre-intent restriction includes strict native scope; it grants no SQL effect,
    /// journal decision, physical qualification or required commit permit.
    pub fn with_lifecycle_policy(mut self) -> Self {
        self.lifecycle_allocation = true;
        self.lifecycle_policy = true;
        self.wal_binding = true;
        self
    }

    /// Request exact original native MAIN/WAL/SHM scope and bounded shared reply custody.
    /// This diagnostic route grants no decision latch, allocation policy or durable intent.
    pub fn with_wal_binding(mut self) -> Self {
        self.wal_binding = true;
        self
    }

    /// Request exact finite lifecycle allocation observations under this empty reservation.
    /// Unknown schema/native objects refuse; this selects no SQL effect or physical permit.
    pub fn with_lifecycle_allocation(mut self) -> Self {
        self.lifecycle_allocation = true;
        self
    }

    /// Form a delayed preparation from an actual historical observation. This comparison
    /// conveys no authority: the current owner and held transaction must validate it again.
    pub fn from_observation(
        observation: &GlobalPreparationObservation,
        command_digest: [u8; 32],
        deadline: Instant,
        affected_databases: usize,
        handshake_bytes: usize,
    ) -> Result<Self, GlobalPreparationError> {
        let mut request = Self::new(
            observation.owner().clone(),
            command_digest,
            deadline,
            affected_databases,
            handshake_bytes,
        )?;
        request.expected_mutation_revision = Some(observation.mutation_revision());
        Ok(request)
    }
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
            expected_mutation_revision: None,
            lifecycle_allocation: false,
            lifecycle_policy: false,
            wal_binding: false,
            handshake: None,
        })
    }
}

/// Copyable diagnostics from the actual retained worker, not a reusable grant or latch.
#[derive(Clone, Debug)]
pub struct GlobalPreparationObservation {
    data: Arc<ObservationData>,
    // Shared with the original worker; final view releases only this local memory credit.
    _binding_credit: Option<Arc<binding::Credit>>,
    _handshake_credit: Option<Arc<binding::SharedHandshakeCredit>>,
}
impl PartialEq for GlobalPreparationObservation {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}
impl Eq for GlobalPreparationObservation {}
#[derive(Debug, Eq, PartialEq)]
struct ObservationData {
    owner: GlobalPreparationOwner,
    database_identity: String,
    command_digest: [u8; 32],
    nonce: uuid::Uuid,
    wal_bytes: u64,
    mutation_revision: u64,
    lifecycle_allocation: Option<crate::lifecycle_allocation::LifecycleDatabaseAllocation>,
    wal_scope: Option<GlobalWalScope>,
    lifecycle_policy_entered: bool,
}
impl GlobalPreparationObservation {
    /// Historical observation that the original worker installed/validated its capped policy.
    /// A copy cannot retain the native reservation or authorize a later write.
    pub fn lifecycle_policy_was_entered(&self) -> bool {
        self.data.lifecycle_policy_entered
    }

    /// Exact historical native scope, when requested on the original actual worker.
    /// Clones share immutable payload and its byte custody, never a live decision grant.
    pub fn wal_scope(&self) -> Option<&GlobalWalScope> {
        self.data.wal_scope.as_ref()
    }

    /// Actual bounded lifecycle facts from this exact held worker, when requested.
    /// Copying observations grants no admission, required commit or capacity qualification.
    pub fn lifecycle_allocation(
        &self,
    ) -> Option<&crate::lifecycle_allocation::LifecycleDatabaseAllocation> {
        self.data.lifecycle_allocation.as_ref()
    }

    /// Checked mutation dispatch revision within this exact worker registration.
    /// It is neither a durable database revision nor a capacity/commit permission.
    pub fn mutation_revision(&self) -> u64 {
        self.data.mutation_revision
    }
    /// Profile acquisition actually validated by the issuing worker.
    pub fn acquisition(&self) -> &ProfileAcquisitionId {
        self.data.owner.acquisition()
    }
    /// Exact worker registration behind this diagnostic.
    pub fn owner(&self) -> &GlobalPreparationOwner {
        &self.data.owner
    }
    /// Descriptor-derived physical identity for coordinator alias/conflict checks.
    pub fn database_identity(&self) -> &str {
        &self.data.database_identity
    }
    /// Immutable command correlation; not proof of a validated journal command.
    pub fn command_digest(&self) -> &[u8; 32] {
        &self.data.command_digest
    }
    /// Owner-issued, non-durable correlation, never an OperationId.
    pub fn nonce(&self) -> uuid::Uuid {
        self.data.nonce
    }
    /// Actual retained WAL size under the empty writer lock.
    pub fn wal_bytes(&self) -> u64 {
        self.data.wal_bytes
    }
    /// Diagnostic request for passive catch-up after release, not a checkpoint request.
    pub fn needs_catch_up(&self) -> bool {
        self.data.wal_bytes >= crate::WAL_SOFT_WATERMARK_BYTES
    }
}

/// A pending delivery. Dropping it cancels queued/held preparation without losing the owner.
pub struct GlobalPreparationPending {
    ready: Receiver<Result<GlobalPreparationReservation, GlobalPreparationError>>,
    canceled: Arc<AtomicBool>,
    armed: bool,
    _handshake_credit: Option<Arc<binding::SharedHandshakeCredit>>,
}
impl GlobalPreparationPending {
    pub(crate) fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
    }
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
    Current(crate::operation_journal::wal_admission::CurrentRequest),
}
/// Live, non-cloneable cancellation handle. It cannot authorize intent or SQL effects.
/// Dropping it requests rollback; ordinary reuse occurs only after confirmed rollback.
pub struct GlobalPreparationReservation {
    observation: GlobalPreparationObservation,
    canceled: Arc<AtomicBool>,
    control: SyncSender<Control>,
    released: Option<Receiver<Result<(), GlobalPreparationError>>>,
    observed_release: Option<Result<(), GlobalPreparationError>>,
}
impl GlobalPreparationReservation {
    pub(crate) fn request_current(
        &self,
        request: crate::operation_journal::wal_admission::CurrentRequest,
    ) -> Result<(), GlobalPreparationError> {
        if self.canceled.load(Ordering::Acquire) {
            return Err(GlobalPreparationError::Canceled);
        }
        self.control
            .try_send(Control::Current(request))
            .map_err(|_| GlobalPreparationError::InputBound)
    }
    pub(crate) fn try_release_outcome(
        &mut self,
    ) -> Result<Option<Result<(), GlobalPreparationError>>, mpsc::TryRecvError> {
        if let Some(result) = &self.observed_release {
            return Ok(Some(result.clone()));
        }
        let Some(receiver) = &self.released else {
            return Ok(None);
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.observed_release = Some(result.clone());
                Ok(Some(result))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Borrow authority-free diagnostics while the owner may still hold the reservation.
    pub fn observation(&self) -> &GlobalPreparationObservation {
        &self.observation
    }
    /// Request abort and obtain confirmation of the empty rollback. A lost response is not
    /// proof of release; the worker still owns reconciliation/connection retirement.
    pub fn abort(mut self) -> Receiver<Result<(), GlobalPreparationError>> {
        self.canceled.store(true, Ordering::Release);
        let _ = self.control.try_send(Control::Abort);
        if let Some(actual_result) = self.observed_release.take() {
            // Forward only the actual original worker result already consumed by internal
            // liveness polling. This cached result cannot be set by a caller or decoder.
            let (sender, receiver) = mpsc::sync_channel(1);
            let _ = sender.try_send(actual_result);
            receiver
        } else {
            self.released.take().expect("single release receiver")
        }
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
    binding_credit: Option<Arc<binding::Credit>>,
    handshake_credit: Option<Arc<binding::SharedHandshakeCredit>>,
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
        self.wal_preparation_client().submit_preparation(request)
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

/// Native SQLite state only: no ordinary query, renewal or new transaction.
/// The baseline is captured on this original connection before BEGIN, never caller facts.
fn validate_empty_transaction(
    connection: &rusqlite::Connection,
    changes: u64,
) -> Result<(), GlobalPreparationError> {
    if connection.is_autocommit()
        || connection.total_changes() != changes
        || connection.transaction_state(Some("main")) != Ok(rusqlite::TransactionState::Write)
    {
        return Err(GlobalPreparationError::EmptyReservationChanged);
    }
    Ok(())
}

/// Returns false only when the connection must close before any further dispatch.
pub(super) fn run(
    command: Command,
    database: &LibraryDatabase,
    guard: &WritableProfileGuard,
    lifecycle: &std::sync::atomic::AtomicU8,
    registration: uuid::Uuid,
    mutation_fence: &mutation_fence::MutationFence,
) -> bool {
    let mut began = false;
    let mut policy = None;
    let preparation = (|| {
        canceled(&command, lifecycle)?;
        if command.request.owner.acquisition.as_str() != guard.owner_identity()
            || command.request.owner.registration != registration
        {
            return Err(GlobalPreparationError::StaleOwner);
        }
        mutation_fence.validate(command.request.expected_mutation_revision)?;
        let mutation_revision = mutation_fence.current();
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
        let original_changes = database.connection.total_changes();
        if command.request.lifecycle_policy {
            policy = Some(
                super::super::lifecycle_allocation::policy::EnteredLifecyclePolicy::new(
                    database,
                    command.request.deadline,
                )?,
            );
            policy
                .as_mut()
                .expect("original lifecycle policy")
                .install()?;
            canceled(&command, lifecycle)?;
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
        validate_empty_transaction(&database.connection, original_changes)?;
        validate(database, guard, &file)?;
        canceled(&command, lifecycle)?;
        let wal_bytes =
            crate::sqlite_wal::enforce_wal_admission(&database.connection)?.observed_bytes();
        canceled(&command, lifecycle)?;
        mutation_fence.validate(Some(mutation_revision))?;
        let lifecycle_allocation = if let Some(policy) = &policy {
            policy.validate_reserved(command.request.deadline)?;
            Some(policy.allocation().clone())
        } else if command.request.lifecycle_allocation {
            Some(database.capture_lifecycle_allocation(command.request.deadline)?)
        } else {
            None
        };
        canceled(&command, lifecycle)?;
        validate(database, guard, &file)?;
        validate_empty_transaction(&database.connection, original_changes)?;
        let nonce = uuid::Uuid::new_v4();
        let wal_scope = if command.request.wal_binding {
            Some(binding::capture(
                database,
                &command.request,
                nonce,
                mutation_revision,
                &file.identity,
                lifecycle_allocation.as_ref(),
            )?)
        } else {
            None
        };
        canceled(&command, lifecycle)?;
        validate_empty_transaction(&database.connection, original_changes)?;
        let (control, controls) = mpsc::sync_channel(1);
        let (released, release_rx) = mpsc::sync_channel(1);
        let observation = GlobalPreparationObservation {
            data: Arc::new(ObservationData {
                owner: command.request.owner.clone(),
                database_identity: file.identity.clone(),
                command_digest: command.request.command_digest,
                nonce,
                wal_bytes,
                mutation_revision,
                lifecycle_allocation,
                wal_scope,
                lifecycle_policy_entered: policy.is_some(),
            }),
            _binding_credit: command.binding_credit.as_ref().map(Arc::clone),
            _handshake_credit: command.handshake_credit.as_ref().map(Arc::clone),
        };
        if let Some(credit) = &command.handshake_credit {
            // Capture staging is over. Keep complete retained data, headers and conservative
            // original parked-native scratch charged before publishing to another owner.
            credit.retain(database, &observation)?;
        }
        // Original worker keeps the same immutable scope and byte custody until rollback.
        let retained_observation = observation.clone();
        let reservation = GlobalPreparationReservation {
            observation,
            canceled: Arc::clone(&command.canceled),
            control,
            released: Some(release_rx),
            observed_release: None,
        };
        if command.ready.try_send(Ok(reservation)).is_err() {
            command.canceled.store(true, Ordering::Release);
        }
        #[cfg(test)]
        tests::inject_empty_custody_fault(&database.connection);
        // No SQL/row query/checkpoint/renewal/ordinary dispatch while held. The main-file FD
        // remains alive, and actual owner/path/native binding is rechecked cooperatively.
        let mut current: Option<crate::operation_journal::wal_admission::CurrentRequest> = None;
        let outcome = loop {
            if let Err(error) = canceled(&command, lifecycle) {
                break error;
            }
            if let Err(error) = validate_empty_transaction(&database.connection, original_changes) {
                break error;
            }
            if let Err(error) = mutation_fence.validate(Some(mutation_revision)) {
                break error;
            }
            if let Err(error) = validate(database, guard, &file) {
                break error;
            }
            if let Some(scope) = retained_observation.wal_scope()
                && let Err(error) = binding::validate(database, scope, command.request.deadline)
            {
                break error;
            }
            if let Some(policy) = &policy
                && let Err(error) = policy.validate_settings()
            {
                break GlobalPreparationError::LifecycleAllocation(error);
            }
            if let Some(request) = current.take() {
                if let Err(error) = canceled(&command, lifecycle) {
                    break error;
                }
                request.acknowledge();
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
                Ok(Control::Current(request)) => {
                    current = Some(request);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        // Cancellation/deadline can win before the next parked native poll. Revalidate
        // original binding while still held before deciding reuse; this cleanup allowance
        // does not extend preparation or publish another acknowledgement.
        let cleanup_empty = validate_empty_transaction(&database.connection, original_changes);
        let cleanup_binding = cleanup_empty.clone().and_then(|_| {
            retained_observation.wal_scope().map_or(Ok(()), |scope| {
                validate(database, guard, &file).and_then(|_| {
                    binding::validate(
                        database,
                        scope,
                        Instant::now() + GLOBAL_PREPARATION_MAX_DURATION,
                    )
                })
            })
        });
        let rollback = database.connection.execute_batch("ROLLBACK");
        began = !database.connection.is_autocommit();
        let policy_restored = policy.as_mut().is_none_or(|policy| policy.finish().is_ok());
        let reusable = rollback.is_ok()
            && !began
            && policy_restored
            && cleanup_binding.is_ok()
            && outcome != GlobalPreparationError::DatabaseBinding
            && outcome != GlobalPreparationError::EmptyReservationChanged;
        let result = if rollback.is_err() || began || !policy_restored {
            Err(GlobalPreparationError::ConnectionRetired)
        } else if let Err(error) = cleanup_binding {
            Err(error)
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
            let policy_restored = policy.as_mut().is_none_or(|policy| policy.finish().is_ok());
            let reusable = rollback_ok
                && policy_restored
                && error != GlobalPreparationError::ConnectionRetired
                && error != GlobalPreparationError::DatabaseBinding
                && error != GlobalPreparationError::EmptyReservationChanged;
            let _ = command
                .ready
                .try_send(Err(if rollback_ok && policy_restored {
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

#[cfg(test)]
mod lifecycle_policy_tests;
