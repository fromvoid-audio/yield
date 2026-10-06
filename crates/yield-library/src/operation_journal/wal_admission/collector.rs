//! One original journal actor polls original owners; no SQLite connection crosses this route.
use super::input::ClaimStorage;
use super::{HandshakeCredit, WalPreparationError, WalPreparationInput};
use crate::sample_sources::{
    library::{
        GlobalPreparationObservation, GlobalPreparationPending, GlobalPreparationReservation,
    },
    wal_transport::{
        SourceLeaseWalTransportObservation, SourceWalTransportCorrelation,
        SourceWalTransportObservation, SourceWalTransportRequest, SourceWalTransportReservation,
        SourceWalTransportStatus,
    },
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::TryRecvError,
    },
    time::Instant,
};
mod retirement_policy;

/// Copied historical scope from one original owner. It contains no live acknowledgement.
#[derive(Clone, Debug)]
pub enum WalPreparedDatabase {
    /// Original global worker scope.
    Global(GlobalPreparationObservation),
    /// Original audited source worker scope.
    Source(SourceWalTransportObservation),
    /// Original pre-audit lease source worker scope.
    Lease(SourceLeaseWalTransportObservation),
}
impl WalPreparedDatabase {
    /// Borrow original historical physical database identity; this is not current custody.
    pub fn database_identity(&self) -> &str {
        match self {
            Self::Global(observation) => observation.database_identity(),
            Self::Source(observation) => observation.revision().database_identity(),
            Self::Lease(observation) => observation
                .original_binding()
                .stored_fence()
                .subject()
                .database_identity(),
        }
    }
}
struct Snapshot {
    databases: Vec<WalPreparedDatabase>,
    _credit: HandshakeCredit,
    _journal_input: Arc<super::super::owner::InputPermit>,
}
/// Immutable original-owner historical preparation inventory. Copies retain all byte debits.
/// It cannot latch a journal decision, authorize capacity or settle an operation.
/// ```compile_fail
/// use yield_library::operation_journal::owner::WalPreparationObservation;
/// let fabricated = WalPreparationObservation {};
/// ```
/// ```compile_fail
/// use yield_library::operation_journal::owner::WalPreparationObservation;
/// fn decode<T: serde::de::DeserializeOwned>() {}
/// decode::<WalPreparationObservation>();
/// ```
#[derive(Clone)]
pub struct WalPreparationObservation(Arc<Snapshot>);
impl std::fmt::Debug for WalPreparationObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalPreparationObservation")
            .field("databases", &self.0.databases.len())
            .finish()
    }
}
impl WalPreparationObservation {
    /// Borrow historical original scope in actual acquisition order.
    pub fn databases(&self) -> &[WalPreparedDatabase] {
        &self.0.databases
    }
}
/// Observed pre-intent collection/cleanup state, never durable admission or settlement.
#[derive(Clone, Debug)]
pub enum WalPreparationStatus {
    /// Original owners are being acquired in total order.
    Preparing,
    /// All original scope replies arrived; these are historical, nonauthoritative observations.
    Ready(WalPreparationObservation),
    /// Cancellation/refusal requests are unwinding the actual acquired prefix.
    Aborting,
    /// Every acquired original owner confirmed empty rollback/reuse.
    Aborted,
    /// Pre-intent refusal after confirmed prefix cleanup.
    Rejected(WalPreparationError),
    /// An original cleanup/owner result remains unconfirmed.
    Unavailable(WalPreparationError),
}
struct Shared {
    canceled: AtomicBool,
    status: Mutex<WalPreparationStatus>,
}
/// Sole non-Clone cancellation handle. Only the original journal actor can publish outcomes.
/// A copied observation cannot preserve current custody or acknowledge settlement.
/// ```compile_fail
/// use yield_library::operation_journal::owner::WalPreparationHandle;
/// let fabricated = WalPreparationHandle {};
/// ```
/// ```compile_fail
/// fn copy(handle: &yield_library::operation_journal::owner::WalPreparationHandle) {
///     let _: yield_library::operation_journal::owner::WalPreparationHandle = handle.clone();
/// }
/// ```
/// ```compile_fail
/// fn settle(handle: &yield_library::operation_journal::owner::WalPreparationHandle) {
///     handle.acknowledge_settlement();
/// }
/// ```
pub struct WalPreparationHandle(Arc<Shared>);
impl WalPreparationHandle {
    /// Observe bounded collection/cleanup without waiting for SQLite or another owner.
    pub fn status(&self) -> WalPreparationStatus {
        self.0
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Request actual reverse prefix cleanup. This is not confirmed rollback or cancellation.
    pub fn abort(&self) {
        self.0.canceled.store(true, Ordering::Release);
    }
}
impl Drop for WalPreparationHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

pub(super) enum HeldOwner {
    Global(GlobalPreparationReservation),
    Source(SourceWalTransportReservation),
}
enum PendingOwner {
    Global(GlobalPreparationPending),
    Source {
        request: SourceWalTransportRequest,
        expected: SourceWalTransportCorrelation,
        storage: ClaimStorage,
    },
}
enum Releasing {
    Global(
        std::sync::mpsc::Receiver<
            Result<(), crate::sample_sources::library::GlobalPreparationError>,
        >,
    ),
    Source(SourceWalTransportReservation),
    PendingSource(SourceWalTransportRequest),
}
/// Actor-local state. Original command backing/handles drop before its journal input debit.
pub(in crate::operation_journal) struct Collector {
    input: WalPreparationInput,
    profile_acquisition: uuid::Uuid,
    shared: Arc<Shared>,
    decision: Option<super::OriginalDecision>,
    held: Vec<HeldOwner>,
    awaiting: Option<PendingOwner>,
    releasing: Option<Releasing>,
    reason: Option<WalPreparationError>,
    global_started: bool,
    snapshot: Option<WalPreparationObservation>,
    current: Option<Arc<super::CurrentRound>>,
    done: bool,
    _journal_input: Arc<super::super::owner::InputPermit>,
}
impl Collector {
    pub(in crate::operation_journal) fn new(
        input: WalPreparationInput,
        journal_input: super::super::owner::InputPermit,
        profile_acquisition: uuid::Uuid,
    ) -> (Self, WalPreparationHandle) {
        let shared = Arc::new(Shared {
            canceled: AtomicBool::new(false),
            status: Mutex::new(WalPreparationStatus::Preparing),
        });
        let capacity = input.sources.len() + usize::from(input.global.is_some());
        let held = Vec::with_capacity(capacity);
        (
            Self {
                input,
                profile_acquisition,
                shared: Arc::clone(&shared),
                decision: None,
                held,
                awaiting: None,
                releasing: None,
                reason: None,
                global_started: false,
                snapshot: None,
                current: None,
                done: false,
                _journal_input: Arc::new(journal_input),
            },
            WalPreparationHandle(shared),
        )
    }
    fn bind_source_round(&self, round: &super::CurrentRound) -> Result<(), WalPreparationError> {
        for (index, owner) in self.held.iter().enumerate() {
            let HeldOwner::Source(holder) = owner else {
                continue;
            };
            let registration = match holder.status() {
                SourceWalTransportStatus::Prepared(view) => view
                    .original_binding()
                    .and_then(|binding| binding.subject().registration())
                    .map(|registration| registration.registration_id()),
                SourceWalTransportStatus::PreparedLease(view) => view
                    .original_binding()
                    .stored_fence()
                    .subject()
                    .registration()
                    .map(|registration| registration.registration_id()),
                _ => None,
            }
            .ok_or(WalPreparationError::InvalidInput)?;
            if !round.require_source(index, registration) {
                return Err(WalPreparationError::InvalidInput);
            }
        }
        Ok(())
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::operation_journal) fn round_for_tests(&self) -> Arc<super::CurrentRound> {
        Arc::clone(self.current.as_ref().unwrap())
    }
    /// Request a new original commit-acknowledgement round; earlier Ready is insufficient.
    pub(in crate::operation_journal) fn begin_decision(
        &mut self,
        expected_digest: [u8; 32],
    ) -> Result<(), WalPreparationError> {
        if expected_digest != self.input.digest
            || self.snapshot.is_none()
            || self.reason.is_some()
            || self.decision.is_some()
            || !self.current.as_ref().is_some_and(|round| round.complete())
        {
            return Err(WalPreparationError::InvalidInput);
        }
        let mut credit = self.input.budget.credit();
        credit
            .resize(super::CurrentRound::retained_bytes(
                self.held
                    .iter()
                    .filter(|owner| matches!(owner, HeldOwner::Source(_)))
                    .count(),
            ))
            .map_err(|_| WalPreparationError::Full)?;
        let (round, decision) =
            super::CurrentRound::decision_round(self.held.len(), self.input.deadline, credit)
                .ok_or(WalPreparationError::InvalidInput)?;
        self.bind_source_round(&round)?;
        for (index, owner) in self.held.iter().enumerate() {
            let result = match owner {
                HeldOwner::Global(holder) => holder
                    .request_current(round.request(index))
                    .map_err(WalPreparationError::Global),
                HeldOwner::Source(holder) => holder
                    .request_current(round.request(index))
                    .map_err(WalPreparationError::Source),
            };
            if let Err(error) = result {
                // The sole guard resolves NoAttempt before reverse prefix cleanup begins.
                drop(decision);
                self.fail(error.clone());
                return Err(error);
            }
        }
        self.current = Some(round);
        self.decision = Some(decision);
        Ok(())
    }
    pub(in crate::operation_journal) fn decision_acknowledged(&self) -> bool {
        self.decision
            .as_ref()
            .is_some_and(|decision| decision.acknowledged())
    }
    pub(in crate::operation_journal) fn decision_nonce(&self) -> Option<uuid::Uuid> {
        self.decision.as_ref().map(|decision| decision.nonce())
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::operation_journal) fn start_settlement(&mut self) -> bool {
        self.decision
            .as_mut()
            .is_some_and(|decision| decision.start_sqlite())
    }
    /// Call only after definite original SQLite outcome or exact durable readback.
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::operation_journal) fn resolve_settlement(&mut self) {
        if let Some(decision) = self.decision.take() {
            decision.resolve();
        }
        self.cancel();
    }
    pub(in crate::operation_journal) fn refuse_decision(&mut self, reason: WalPreparationError) {
        // Drop before SQL is definitive NoAttempt. After SQL, refuse cannot release custody.
        if self
            .decision
            .as_ref()
            .is_some_and(|decision| decision.sqlite_started())
        {
            return;
        }
        drop(self.decision.take());
        self.fail(reason);
    }
    pub(in crate::operation_journal) fn is_unavailable(&self) -> bool {
        matches!(
            *self.shared.status.lock().unwrap_or_else(|p| p.into_inner()),
            WalPreparationStatus::Unavailable(_)
        )
    }
    fn publish(&self, status: WalPreparationStatus) {
        *self.shared.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    }
    pub(in crate::operation_journal) fn cancel(&self) {
        self.shared.canceled.store(true, Ordering::Release);
    }
    fn fail(&mut self, reason: WalPreparationError) {
        if self.reason.is_none() {
            self.reason = Some(reason);
        }
        self.cancel();
        self.publish(WalPreparationStatus::Aborting);
        self.snapshot = None;
    }
    /// Return true only after the collector no longer has outstanding owner cleanup.
    pub(in crate::operation_journal) fn poll(&mut self) -> bool {
        if self
            .decision
            .as_ref()
            .is_some_and(|decision| decision.sqlite_started())
        {
            // No timer, dropped handle or owner shutdown proves a journal transaction outcome.
            return false;
        }
        if self.decision.is_some()
            && (self.shared.canceled.load(Ordering::Acquire)
                || Instant::now() >= self.input.deadline)
        {
            drop(self.decision.take());
        }
        if self.shared.canceled.load(Ordering::Acquire) {
            if self.reason.is_none() {
                self.reason = Some(WalPreparationError::Canceled);
            }
            self.publish(WalPreparationStatus::Aborting);
        } else if Instant::now() >= self.input.deadline {
            self.fail(WalPreparationError::Deadline);
        }
        if self.reason.is_some() {
            return self.poll_abort();
        }
        if self
            .current
            .as_ref()
            .is_some_and(|round| round.invalidated())
        {
            // Registration revocation can invalidate an ACK without native cleanup.
            // Before SQLite starts, release the sole decision guard before abort polling.
            drop(self.decision.take());
            self.fail(WalPreparationError::OwnerLost);
            return self.poll_abort();
        }
        // Native producer status is current; the immutable snapshot itself is not.
        for owner in &mut self.held {
            match owner {
                HeldOwner::Source(holder) => match holder.status() {
                    SourceWalTransportStatus::Prepared(_)
                    | SourceWalTransportStatus::PreparedLease(_) => {}
                    _ => {
                        self.fail(WalPreparationError::OwnerLost);
                        return self.poll_abort();
                    }
                },
                HeldOwner::Global(holder) => match holder.try_release_outcome() {
                    Ok(None) => {}
                    _ => {
                        self.fail(WalPreparationError::OwnerLost);
                        return self.poll_abort();
                    }
                },
            }
        }
        if self.decision.is_some() {
            // Fresh commit ACK custody has its own state. Do not republish cached
            // Ready or ask the earlier Preparing round to complete this decision.
            return false;
        }
        if let Some(pending) = self.awaiting.take() {
            match pending {
                PendingOwner::Global(mut pending) => match pending.try_recv() {
                    Err(TryRecvError::Empty) => {
                        self.awaiting = Some(PendingOwner::Global(pending));
                        return false;
                    }
                    Err(_) => {
                        self.fail(WalPreparationError::OwnerLost);
                        return self.poll_abort();
                    }
                    Ok(Err(error)) => {
                        self.fail(WalPreparationError::Global(error));
                        return self.poll_abort();
                    }
                    Ok(Ok(holder)) => {
                        let observation = holder.observation();
                        if uuid::Uuid::parse_str(observation.owner().acquisition().as_str()).ok()
                            != Some(self.profile_acquisition)
                            || observation.command_digest() != &self.input.digest
                            || observation.wal_scope().is_none_or(|scope| {
                                Some(ClaimStorage {
                                    device: scope.main().device(),
                                    unit: scope.main().allocation_unit(),
                                }) != self.input.global_storage
                            })
                            || self.input.sources.iter().any(|source| {
                                source.input.database_identity() == observation.database_identity()
                            })
                        {
                            self.held.push(HeldOwner::Global(holder));
                            self.fail(WalPreparationError::InvalidInput);
                            return self.poll_abort();
                        }
                        self.held.push(HeldOwner::Global(holder));
                    }
                },
                PendingOwner::Source {
                    mut request,
                    expected,
                    storage,
                } => match request.status() {
                    SourceWalTransportStatus::Pending | SourceWalTransportStatus::Aborting => {
                        self.awaiting = Some(PendingOwner::Source {
                            request,
                            expected,
                            storage,
                        });
                        return false;
                    }
                    SourceWalTransportStatus::Prepared(ref observation) => {
                        if !observation.registered_custody_was_retained()
                            || observation.correlation() != &expected
                            || observation.original_binding().is_none_or(|binding| {
                                binding.main().device() != storage.device
                                    || binding.main().allocation_unit() != storage.unit
                                    || binding.subject().profile_acquisition()
                                        != self.profile_acquisition
                            })
                            || self.alias(observation.revision().database_identity())
                        {
                            request.cancel();
                            self.awaiting = Some(PendingOwner::Source {
                                request,
                                expected,
                                storage,
                            });
                            self.fail(WalPreparationError::InvalidInput);
                            return self.poll_abort();
                        }
                        self.held.push(HeldOwner::Source(
                            request.try_prepared().expect("original prepared transfer"),
                        ));
                    }
                    SourceWalTransportStatus::PreparedLease(ref observation) => {
                        if !observation.registered_custody_was_retained()
                            || observation.correlation() != &expected
                            || observation
                                .original_binding()
                                .stored_fence()
                                .database_device()
                                != storage.device
                            || observation
                                .original_binding()
                                .stored_fence()
                                .allocation_unit()
                                != storage.unit
                            || observation
                                .original_binding()
                                .stored_fence()
                                .subject()
                                .profile_acquisition()
                                != self.profile_acquisition
                            || self.alias(
                                observation
                                    .original_binding()
                                    .stored_fence()
                                    .subject()
                                    .database_identity(),
                            )
                        {
                            request.cancel();
                            self.awaiting = Some(PendingOwner::Source {
                                request,
                                expected,
                                storage,
                            });
                            self.fail(WalPreparationError::InvalidInput);
                            return self.poll_abort();
                        }
                        self.held.push(HeldOwner::Source(
                            request.try_prepared().expect("original lease transfer"),
                        ));
                    }
                    SourceWalTransportStatus::Rejected(error)
                    | SourceWalTransportStatus::Unavailable(error) => {
                        request.cancel();
                        self.awaiting = Some(PendingOwner::Source {
                            request,
                            expected,
                            storage,
                        });
                        self.fail(WalPreparationError::Source(error));
                        return self.poll_abort();
                    }
                    _ => {
                        self.fail(WalPreparationError::OwnerLost);
                        return self.poll_abort();
                    }
                },
            }
        }
        if !self.global_started {
            self.global_started = true;
            if let Some((client, request)) = self.input.global.take() {
                match client.submit_preparation(request) {
                    Ok(pending) => {
                        self.awaiting = Some(PendingOwner::Global(pending));
                        return false;
                    }
                    Err(crate::sample_sources::library::GlobalLibraryWriterQueueError::Full) => {
                        self.fail(WalPreparationError::Full);
                        return self.poll_abort();
                    }
                    Err(_) => {
                        self.fail(WalPreparationError::OwnerLost);
                        return self.poll_abort();
                    }
                }
            }
        }
        if let Some(source) = self.input.sources.pop() {
            if source
                .profile_acquisition
                .is_some_and(|a| a != self.profile_acquisition)
            {
                self.fail(WalPreparationError::InvalidInput);
                return self.poll_abort();
            }
            let storage = source
                .database_storage
                .expect("original validated claim volume");
            let correlation = match SourceWalTransportCorrelation::new(
                self.input.digest,
                source.participant,
                source.database_claim,
                source.sidecar_claim,
            ) {
                Ok(correlation) => correlation,
                Err(error) => {
                    self.fail(WalPreparationError::Source(error));
                    return self.poll_abort();
                }
            };
            match source.requester.submit_in_handshake(
                source.input,
                correlation.clone(),
                self.input.deadline,
                Arc::clone(&self.input.budget),
            ) {
                Ok(request) => {
                    let Some(bytes) = self.input.retained_bytes() else {
                        request.cancel();
                        self.awaiting = Some(PendingOwner::Source {
                            request,
                            expected: correlation,
                            storage,
                        });
                        self.fail(WalPreparationError::Full);
                        return self.poll_abort();
                    };
                    if self.input.credit.resize(bytes).is_err() {
                        request.cancel();
                        self.awaiting = Some(PendingOwner::Source {
                            request,
                            expected: correlation,
                            storage,
                        });
                        self.fail(WalPreparationError::Full);
                        return self.poll_abort();
                    }
                    self.awaiting = Some(PendingOwner::Source {
                        request,
                        expected: correlation,
                        storage,
                    });
                    return false;
                }
                Err(error) => {
                    self.fail(WalPreparationError::Source(error));
                    return self.poll_abort();
                }
            }
        }
        if self.snapshot.is_none() && !self.held.is_empty() {
            if self.current.is_none() {
                let mut credit = self.input.budget.credit();
                if credit
                    .resize(super::CurrentRound::retained_bytes(
                        self.held
                            .iter()
                            .filter(|owner| matches!(owner, HeldOwner::Source(_)))
                            .count(),
                    ))
                    .is_err()
                {
                    self.fail(WalPreparationError::Full);
                    return self.poll_abort();
                }
                let Some(round) =
                    super::CurrentRound::new(self.held.len(), self.input.deadline, credit)
                else {
                    self.fail(WalPreparationError::InvalidInput);
                    return self.poll_abort();
                };
                if let Err(error) = self.bind_source_round(&round) {
                    self.fail(error);
                    return self.poll_abort();
                }
                for (index, owner) in self.held.iter().enumerate() {
                    let result = match owner {
                        HeldOwner::Global(holder) => holder
                            .request_current(round.request(index))
                            .map_err(WalPreparationError::Global),
                        HeldOwner::Source(holder) => holder
                            .request_current(round.request(index))
                            .map_err(WalPreparationError::Source),
                    };
                    if let Err(error) = result {
                        self.fail(error);
                        return self.poll_abort();
                    }
                }
                self.current = Some(round);
                return false;
            }
            if !self
                .current
                .as_ref()
                .expect("original validation round")
                .complete()
            {
                return false;
            }
        }
        if self.snapshot.is_none() {
            match self.make_snapshot() {
                Ok(snapshot) => {
                    if let Some(round) = &self.current {
                        if round
                            .with_complete(|| {
                                self.publish(WalPreparationStatus::Ready(snapshot.clone()))
                            })
                            .is_none()
                        {
                            self.fail(WalPreparationError::OwnerLost);
                            return self.poll_abort();
                        }
                    } else {
                        self.publish(WalPreparationStatus::Ready(snapshot.clone()));
                    }
                    self.snapshot = Some(snapshot);
                }
                Err(error) => {
                    self.fail(error);
                    return self.poll_abort();
                }
            }
        }
        false
    }
    fn alias(&self, identity: &str) -> bool {
        self.held.iter().any(|owner| match owner {
            HeldOwner::Global(holder) => holder.observation().database_identity() == identity,
            HeldOwner::Source(holder) => match holder.status() {
                SourceWalTransportStatus::Prepared(observation) => {
                    observation.revision().database_identity() == identity
                }
                SourceWalTransportStatus::PreparedLease(observation) => {
                    observation
                        .original_binding()
                        .stored_fence()
                        .subject()
                        .database_identity()
                        == identity
                }
                _ => true,
            },
        })
    }
    fn make_snapshot(&self) -> Result<WalPreparationObservation, WalPreparationError> {
        let mut credit = self.input.budget.credit();
        let bytes = std::mem::size_of::<Snapshot>()
            .checked_add(64)
            .and_then(|n| {
                n.checked_add(
                    self.held
                        .len()
                        .checked_mul(std::mem::size_of::<WalPreparedDatabase>())?,
                )
            })
            .ok_or(WalPreparationError::Full)?;
        credit
            .resize(bytes)
            .map_err(|_| WalPreparationError::Full)?;
        let mut databases = Vec::with_capacity(self.held.len());
        for owner in &self.held {
            databases.push(match owner {
                HeldOwner::Global(holder) => {
                    WalPreparedDatabase::Global(holder.observation().clone())
                }
                HeldOwner::Source(holder) => match holder.status() {
                    SourceWalTransportStatus::Prepared(observation) => {
                        WalPreparedDatabase::Source(observation)
                    }
                    SourceWalTransportStatus::PreparedLease(observation) => {
                        WalPreparedDatabase::Lease(observation)
                    }
                    _ => return Err(WalPreparationError::OwnerLost),
                },
            });
        }
        Ok(WalPreparationObservation(Arc::new(Snapshot {
            databases,
            _credit: credit,
            _journal_input: Arc::clone(&self._journal_input),
        })))
    }
    fn poll_abort(&mut self) -> bool {
        if let Some(pending) = self.awaiting.take() {
            match pending {
                PendingOwner::Global(mut pending) => match pending.try_recv() {
                    Ok(Ok(holder)) => self.held.push(HeldOwner::Global(holder)),
                    Ok(Err(
                        crate::sample_sources::library::GlobalPreparationError::ConnectionRetired
                        | crate::sample_sources::library::GlobalPreparationError::DatabaseBinding,
                    ))
                    | Err(TryRecvError::Disconnected) => {
                        self.reason = Some(WalPreparationError::OwnerLost);
                    }
                    Ok(Err(_)) => {}
                    Err(TryRecvError::Empty) => {
                        pending.cancel();
                        self.awaiting = Some(PendingOwner::Global(pending));
                        return false;
                    }
                },
                PendingOwner::Source { request, .. } => {
                    request.cancel();
                    self.releasing = Some(Releasing::PendingSource(request));
                }
            }
        }
        if let Some(releasing) = self.releasing.take() {
            match releasing {
                Releasing::Global(receiver) => match receiver.try_recv() {
                    Ok(Ok(())) => {}
                    Ok(Err(crate::sample_sources::library::GlobalPreparationError::Deadline)) => {
                        if self.reason == Some(WalPreparationError::Canceled) {
                            self.reason = Some(WalPreparationError::Deadline);
                        }
                    }
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                        self.reason = Some(WalPreparationError::OwnerLost);
                    }
                    Err(TryRecvError::Empty) => {
                        self.releasing = Some(Releasing::Global(receiver));
                        return false;
                    }
                },
                Releasing::Source(holder) => match holder.status() {
                    SourceWalTransportStatus::Aborted | SourceWalTransportStatus::Rejected(_) => {}
                    SourceWalTransportStatus::Unavailable(_)
                    | SourceWalTransportStatus::Retired => {
                        self.reason = Some(WalPreparationError::OwnerLost);
                    }
                    _ => {
                        holder.abort();
                        self.releasing = Some(Releasing::Source(holder));
                        return false;
                    }
                },
                Releasing::PendingSource(request) => match request.status() {
                    SourceWalTransportStatus::Aborted | SourceWalTransportStatus::Rejected(_) => {}
                    SourceWalTransportStatus::Unavailable(_)
                    | SourceWalTransportStatus::Retired => {
                        self.reason = Some(WalPreparationError::OwnerLost);
                    }
                    _ => {
                        request.cancel();
                        self.releasing = Some(Releasing::PendingSource(request));
                        return false;
                    }
                },
            }
        }
        if let Some(owner) = self.held.pop() {
            self.releasing = Some(match owner {
                HeldOwner::Global(holder) => Releasing::Global(holder.abort()),
                HeldOwner::Source(holder) => {
                    holder.abort();
                    Releasing::Source(holder)
                }
            });
            return false;
        }
        self.snapshot = None;
        let reason = self.reason.clone().unwrap_or(WalPreparationError::Canceled);
        self.publish(if reason == WalPreparationError::OwnerLost {
            WalPreparationStatus::Unavailable(reason)
        } else if reason == WalPreparationError::Canceled {
            WalPreparationStatus::Aborted
        } else {
            WalPreparationStatus::Rejected(reason)
        });
        self.done = true;
        true
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        if !self.done {
            self.cancel();
            // Publish loss before original cancellation handles/native resources can drop.
            self.publish(WalPreparationStatus::Unavailable(
                WalPreparationError::OwnerLost,
            ));
        }
    }
}
