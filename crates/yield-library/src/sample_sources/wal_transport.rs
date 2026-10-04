//! Sealed pre-intent requester/producer transport for the original actual source owner.
//! No request, observation or cancellation holder can acknowledge journal settlement.
use crate::operation_journal::{AdmissionClaimId, ParticipantId};
use crate::sample_sources::db::write::{
    SourceWalPreparation, SourceWalPreparationError, SourceWalPreparationFailure,
};
use crate::sample_sources::db::{
    SourceAuthorityError, SourceAuthorityRegistration, SourceAuthorityRetirementAck,
};
use crate::sample_sources::{SourceRevision, SourceWriter};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
mod lease;
pub use lease::{
    SourceLeaseWalTransportCommand, SourceLeaseWalTransportObservation,
    SourceLeaseWalTransportOwner,
};
mod dispatch;
pub use dispatch::{SourceWalProducerCommand, SourceWalProducerOwner};
mod policy;
mod producer;
pub use policy::SourceWalPolicyFamily;
use policy::{Native, RequestPolicy};
mod scope;
pub use producer::{SourceWalOwnerObservation, SourceWalTransportOwner};
pub(crate) use scope::CAPTURE_BYTES as VALIDATION_STAGING_BYTES;
use scope::{ByteCredit, ScopeView};

/// Maximum pending plus held source preparation slots for one catalog worker.
pub const SOURCE_WAL_TRANSPORT_MAX_SLOTS: usize = 64;
/// Aggregate retained handshake identity/correlation bytes, independently of normal commands.
pub const SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_DEADLINE: Duration = Duration::from_secs(2);
// Charge complete retained payloads plus their concrete headers/observations and bounded
// owner bookkeeping. The original SourceWriter/database, snapshots and watcher already
// existed in RegisteredSource and move without duplication. Empty queue backing stores
// no handshake payload; its separate allocation is bounded by the same 64-slot limit.
const CONTEXT_FIXED_BYTES: usize = std::mem::size_of::<PrepareRequest>()
    + std::mem::size_of::<SourceWalTransportOwner>()
        .saturating_sub(std::mem::size_of::<SourceWriter>())
    + std::mem::size_of::<Ticket>()
    + std::mem::size_of::<Credit>()
    + std::mem::size_of::<SourceWalPreparation>()
        .saturating_sub(std::mem::size_of::<SourceWriter>())
    + 160; // Arc headers, slot deadline/cancellation, scope option tags, nonce and map bookkeeping.

/// Bounded immutable correlations; these IDs do not issue or reserve any capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWalTransportCorrelation {
    command_digest: [u8; 32],
    participant: ParticipantId,
    database_claim: AdmissionClaimId,
    sidecar_claim: AdmissionClaimId,
}
impl SourceWalTransportCorrelation {
    /// Bind one command digest and distinct nonnil DB and WAL/SHM claim observations.
    pub fn new(
        command_digest: [u8; 32],
        participant: ParticipantId,
        database_claim: AdmissionClaimId,
        sidecar_claim: AdmissionClaimId,
    ) -> Result<Self, SourceWalTransportRejection> {
        if command_digest == [0; 32]
            || participant.as_uuid().is_nil()
            || database_claim.validate().is_err()
            || sidecar_claim.validate().is_err()
            || database_claim == sidecar_claim
        {
            return Err(SourceWalTransportRejection::InvalidContext);
        }
        Ok(Self {
            command_digest,
            participant,
            database_claim,
            sidecar_claim,
        })
    }
}

/// Closed pre-intent rejection categories. No paths or raw payloads enter diagnostics.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SourceWalTransportRejection {
    /// Pending/held slot or aggregate context credits are exhausted.
    #[error("catalog source preparation is full")]
    Full,
    /// The catalog worker is closed or shutting down.
    #[error("catalog source preparation owner is closed")]
    Closed,
    /// The original deadline expired or exceeds the two-second preparation bound.
    #[error("catalog source preparation deadline expired")]
    Deadline,
    /// The exact source registration/lifecycle/root/database/revision no longer agrees.
    #[error("catalog source preparation scope changed")]
    Scope,
    /// One exact source already has a retained slot, or a physical database aliases it.
    #[error("catalog source preparation is busy or aliases a held database")]
    Busy,
    /// Cancellation was observed before durable intent; this is not a commit result.
    #[error("catalog source preparation cancelled")]
    Cancelled,
    /// A separately admitted lease renewal is required before preparation.
    #[error("catalog source preparation requires lease renewal")]
    LeaseRenewalRequired,
    /// Retained WAL is hard/unknown or the immediate SQLite writer is unavailable.
    #[error("catalog source preparation WAL or writer is unavailable")]
    WalUnavailable,
    /// Correlation/identity data is invalid or cannot fit the complete finite handshake.
    #[error("catalog source preparation correlation is invalid")]
    InvalidContext,
    /// No pre-intent handle can latch, settle, commit or issue a journal operation.
    #[error("catalog source preparation cannot acknowledge journal settlement")]
    SettlementUnsupported,
    /// The owner was lost without a confirmed reusable writer or source-close acknowledgement.
    #[error("catalog source preparation owner outcome remains unconfirmed")]
    OwnerLost,
}

/// Historical preparation diagnostics, never a live journal decision or publication grant.
///
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceWalTransportObservation;
/// let forged = SourceWalTransportObservation {};
/// ```
#[derive(Clone, Debug)]
pub struct SourceWalTransportObservation {
    data: Arc<ObservationData>,
}
#[derive(Debug)]
struct ObservationData {
    revision: SourceRevision,
    correlation: SourceWalTransportCorrelation,
    nonce: String,
    wal_bytes: u64,
    scope: Option<ScopeView>,
    policy_family: Option<SourceWalPolicyFamily>,
    _credit: ByteCredit,
}
impl PartialEq for SourceWalTransportObservation {
    fn eq(&self, other: &Self) -> bool {
        self.data.revision == other.data.revision
            && self.data.correlation == other.data.correlation
            && self.data.nonce == other.data.nonce
            && self.data.wal_bytes == other.data.wal_bytes
            && self.data.scope == other.data.scope
            && self.data.policy_family == other.data.policy_family
    }
}
impl Eq for SourceWalTransportObservation {}
impl SourceWalTransportObservation {
    /// Historical family marker issued only after the actual owned policy entered successfully.
    /// This never proves current journal registration, physical allocation or settlement.
    pub fn policy_family(&self) -> Option<SourceWalPolicyFamily> {
        self.data.policy_family
    }

    /// Borrow typed historical original scope. It cannot recreate live owner custody or a latch.
    pub fn original_binding(&self) -> Option<&crate::sample_sources::db::write::SourceWalBinding> {
        self.data.scope.as_ref().map(ScopeView::binding)
    }

    /// Shared historical original owner descriptor, when explicitly requested.
    /// This never proves current journal registration or issues settlement authority.
    /// Clones share the buffer and retain its byte credit until the last view is dropped.
    pub fn scope_descriptor_bytes(&self) -> Option<&[u8]> {
        self.data.scope.as_ref().map(ScopeView::bytes)
    }
    /// Complete source revision observed by the original retained writer.
    pub fn revision(&self) -> &SourceRevision {
        &self.data.revision
    }
    /// Retained VFS WAL byte observation, not a current watermark permission.
    pub fn wal_bytes(&self) -> u64 {
        self.data.wal_bytes
    }
    /// Original command/participant/claim observations, not allocated claims.
    pub fn correlation(&self) -> &SourceWalTransportCorrelation {
        &self.data.correlation
    }
    /// Opaque local preparation nonce, not an OperationId.
    pub fn nonce(&self) -> &str {
        &self.data.nonce
    }
}

/// Observed owner-controlled state. Caller clocks and handle drop never confirm rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceWalTransportStatus {
    /// Bounded control was admitted; no SQLite reservation is implied.
    Pending,
    /// Actual writer is parked under the original live pre-intent reservation.
    Prepared(SourceWalTransportObservation),
    /// Original pre-audit lease owner is parked; this cannot fabricate an audited revision.
    PreparedLease(SourceLeaseWalTransportObservation),
    /// Abort has been requested; the writer is still exclusively retained.
    Aborting,
    /// The original writer was returned only after confirmed rollback and timeout restoration.
    Aborted,
    /// The original draining source authority confirmed connection and lease retirement.
    /// Journal finalization may still be pending; this does not mean a reusable writer exists.
    Retired,
    /// Rejection is confirmed without a retained preparation resource.
    Rejected(SourceWalTransportRejection),
    /// Cleanup remains unresolved; the private writer is unavailable for ordinary SQL.
    Unavailable(SourceWalTransportRejection),
}
struct Ticket {
    cancelled: AtomicBool,
    status: Mutex<SourceWalTransportStatus>,
    credit: Mutex<Option<Credit>>,
}
impl Ticket {
    fn status(&self) -> SourceWalTransportStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn set(&self, status: SourceWalTransportStatus) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    }
    fn finish(&self, status: SourceWalTransportStatus) {
        // Owner calls this only after a returned writer or confirmed absence of reservation.
        drop(self.credit.lock().unwrap_or_else(|p| p.into_inner()).take());
        self.set(status);
    }
}

/// Non-clone pending control holder. Drop signals abort, retaining owner-side custody.
pub struct SourceWalTransportRequest {
    ticket: Arc<Ticket>,
    transferred: bool,
}
impl SourceWalTransportRequest {
    /// Read bounded state without SQL, lease validation or timer-driven release.
    pub fn status(&self) -> SourceWalTransportStatus {
        self.ticket.status()
    }
    /// Transfer the sole cancellation holder after the real worker publishes preparation.
    /// A copied observation cannot perform this transfer or acquire any authority.
    pub fn try_prepared(&mut self) -> Option<SourceWalTransportReservation> {
        if self.transferred
            || !matches!(
                self.ticket.status(),
                SourceWalTransportStatus::Prepared(_) | SourceWalTransportStatus::PreparedLease(_)
            )
        {
            return None;
        }
        self.transferred = true;
        Some(SourceWalTransportReservation {
            ticket: Arc::clone(&self.ticket),
        })
    }
    /// Request owner-driven abort. Completion still requires the worker's actual rollback.
    pub fn cancel(&self) {
        if !self.transferred {
            self.ticket.cancelled.store(true, Ordering::Release);
        }
    }
}
impl Drop for SourceWalTransportRequest {
    fn drop(&mut self) {
        if !self.transferred {
            self.cancel();
        }
    }
}
/// Non-clone delivered pre-intent holder, containing no SQLite writer, lease or effect permission.
pub struct SourceWalTransportReservation {
    ticket: Arc<Ticket>,
}
impl SourceWalTransportReservation {
    /// Observe prepared/aborting/terminal custody; only the original worker changes outcomes.
    pub fn status(&self) -> SourceWalTransportStatus {
        self.ticket.status()
    }
    /// Ask the original owner to abort; this does not synchronously unlock or return a writer.
    pub fn abort(&self) {
        self.ticket.cancelled.store(true, Ordering::Release);
    }
    /// Explicitly reject any attempt to use a pre-intent holder as a post-latch decision.
    pub fn acknowledge_settlement(&self) -> Result<(), SourceWalTransportRejection> {
        Err(SourceWalTransportRejection::SettlementUnsupported)
    }
}
impl Drop for SourceWalTransportReservation {
    fn drop(&mut self) {
        self.abort();
    }
}

struct PrepareRequest {
    expected: Option<SourceRevision>,
    correlation: SourceWalTransportCorrelation,
    deadline: Instant,
    ticket: Arc<Ticket>,
    require_binding: bool,
    policy: Option<RequestPolicy>,
    lease: Option<crate::sample_sources::db::write::SourceLeaseMaintenanceAllocation>,
    handshake: Option<Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
}
impl PrepareRequest {
    fn finish(self, status: SourceWalTransportStatus) {
        let ticket = Arc::clone(&self.ticket);
        // Drop boxed policy, selections and revision allocations before refunding input.
        drop(self);
        ticket.finish(status);
    }
}

struct Queue {
    requests: VecDeque<PrepareRequest>,
    slots: usize,
    bytes: usize,
    closed: bool,
}
/// Cloneable bounded pre-intent requester. It contains no SQL writer or publishing method.
#[derive(Clone)]
pub struct SourceWalTransport {
    shared: Arc<Shared>,
}
/// Sole source-actor inbox. Dropping it closes future requests; native producers finish cleanup.
pub struct SourceWalTransportInbox {
    shared: Arc<Shared>,
}
struct Shared {
    queue: Mutex<Queue>,
    #[cfg(feature = "test-support")]
    unknown_wal: AtomicBool,
}
struct Credit {
    shared: Weak<Shared>,
    bytes: usize,
    _handshake: Option<crate::operation_journal::wal_admission::HandshakeCredit>,
}
impl Drop for Credit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            let mut q = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.slots = q
                .slots
                .checked_sub(1)
                .expect("source preparation slot credit");
            q.bytes = q
                .bytes
                .checked_sub(self.bytes)
                .expect("source preparation byte credit");
        }
    }
}
fn close(shared: &Arc<Shared>) {
    let pending = {
        let mut q = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.closed = true;
        q.requests.drain(..).collect::<Vec<_>>()
    };
    for request in pending {
        request.finish(SourceWalTransportStatus::Rejected(
            SourceWalTransportRejection::Closed,
        ));
    }
}
impl SourceWalTransport {
    /// Stop new admission and reject pending commands. Native holders require producer cleanup.
    pub fn close(&self) {
        close(&self.shared);
    }

    /// Create one bounded requester and its non-Clone source-actor inbox.
    /// A channel cannot publish Prepared: the producer must acquire a genuine SourceWriter.
    pub fn channel() -> (Self, SourceWalTransportInbox) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                requests: VecDeque::new(),
                slots: 0,
                bytes: 0,
                closed: false,
            }),
            #[cfg(feature = "test-support")]
            unknown_wal: AtomicBool::new(false),
        });
        (
            Self {
                shared: Arc::clone(&shared),
            },
            SourceWalTransportInbox { shared },
        )
    }
    /// Submit diagnostic-only preparation on the original source actor; no effect authority.
    pub fn submit(
        &self,
        expected: SourceRevision,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        self.submit_scoped(expected, correlation, deadline, false, None, None)
    }
    /// Submit strict original-binding capture; its actor must supply a genuine registration.
    pub fn submit_with_binding(
        &self,
        expected: SourceRevision,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        self.submit_scoped(expected, correlation, deadline, true, None, None)
    }
    /// Consume a genuine tag observation into strict preparation on its original source actor.
    /// Retains the entered finite policy; original observation deadline can never be extended.
    /// No caller settings, decoded policy, journal decision or effect permission is accepted.
    pub fn submit_tag_with_binding(
        &self,
        observation: crate::sample_sources::db::SourceTagAdmissionObservation,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        let deadline = deadline.min(observation.original_deadline());
        let expected = observation.selection().revision().clone();
        self.submit_scoped(
            expected,
            correlation,
            deadline,
            true,
            Some(RequestPolicy::Tag(Box::new(observation))),
            None,
        )
    }
    /// Consume genuine layout allocation observations into original strict owned preparation.
    /// Copies cannot substitute a current owner; the actor revalidates full facts before entry.
    pub fn submit_layout_with_binding(
        &self,
        budget: crate::sample_sources::db::SourceLayoutMetadataBudget,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        let deadline = deadline.min(budget.observation_deadline());
        let expected = budget.revision().clone();
        self.submit_scoped(
            expected,
            correlation,
            deadline,
            true,
            Some(RequestPolicy::Layout(Box::new(budget))),
            None,
        )
    }
    pub(crate) fn submit_in_handshake(
        &self,
        input: crate::operation_journal::wal_admission::SourceInput,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
        handshake: Arc<crate::operation_journal::wal_admission::HandshakeBudget>,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        use crate::operation_journal::wal_admission::SourceInput;
        match input {
            SourceInput::Audited(revision) => {
                self.submit_scoped(revision, correlation, deadline, true, None, Some(handshake))
            }
            SourceInput::Tag(observation) => {
                let deadline = deadline.min(observation.original_deadline());
                let revision = observation.selection().revision().clone();
                self.submit_scoped(
                    revision,
                    correlation,
                    deadline,
                    true,
                    Some(RequestPolicy::Tag(Box::new(observation))),
                    Some(handshake),
                )
            }
            SourceInput::Layout(budget) => {
                let deadline = deadline.min(budget.observation_deadline());
                let revision = budget.revision().clone();
                self.submit_scoped(
                    revision,
                    correlation,
                    deadline,
                    true,
                    Some(RequestPolicy::Layout(Box::new(budget))),
                    Some(handshake),
                )
            }
            SourceInput::Lease(input) => {
                self.submit_lease_scoped(input, correlation, deadline, Some(handshake))
            }
        }
    }
    fn submit_scoped(
        &self,
        expected: SourceRevision,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
        require_binding: bool,
        policy: Option<RequestPolicy>,
        handshake: Option<Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > MAX_DEADLINE {
            return Err(SourceWalTransportRejection::Deadline);
        }
        // Bound the original subject clone before owner capture staging is allocated.
        if require_binding
            && (expected.source_id().as_str().len() > 256
                || expected.root_identity().as_bytes().len() > 4096
                || expected.root_binding_identity().len() > 1024
                || expected.database_identity().len() > 1024)
        {
            return Err(SourceWalTransportRejection::InvalidContext);
        }
        let size = [
            expected.source_id().as_str().len(),
            expected.root_identity().as_bytes().len(),
            expected.root_binding_identity().len(),
            expected.database_identity().len(),
        ]
        .into_iter()
        .try_fold(0usize, |n, len| n.checked_add(len))
        .and_then(|n| n.checked_mul(3))
        .and_then(|n| n.checked_add(CONTEXT_FIXED_BYTES))
        .ok_or(SourceWalTransportRejection::InvalidContext)?;
        let size = size
            .checked_add(match &policy {
                Some(policy) => policy
                    .input_bytes()
                    .ok_or(SourceWalTransportRejection::InvalidContext)?,
                None => 0,
            })
            .ok_or(SourceWalTransportRejection::InvalidContext)?;
        if size > SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES {
            return Err(SourceWalTransportRejection::InvalidContext);
        }
        self.enqueue(
            Some(expected),
            correlation,
            deadline,
            require_binding,
            policy,
            None,
            size,
            handshake,
        )
    }
    fn enqueue(
        &self,
        expected: Option<SourceRevision>,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
        require_binding: bool,
        policy: Option<RequestPolicy>,
        lease: Option<crate::sample_sources::db::write::SourceLeaseMaintenanceAllocation>,
        size: usize,
        handshake: Option<Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        if size > SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES {
            return Err(SourceWalTransportRejection::InvalidContext);
        }
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if q.closed {
            return Err(SourceWalTransportRejection::Closed);
        }
        if q.slots >= SOURCE_WAL_TRANSPORT_MAX_SLOTS
            || q.bytes > SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES - size
        {
            return Err(SourceWalTransportRejection::Full);
        }
        let handshake_credit = handshake
            .as_ref()
            .map(|budget| {
                let mut credit = budget.credit();
                credit
                    .resize(size)
                    .map_err(|_| SourceWalTransportRejection::Full)?;
                Ok(credit)
            })
            .transpose()?;
        q.slots += 1;
        q.bytes += size;
        let ticket = Arc::new(Ticket {
            cancelled: AtomicBool::new(false),
            status: Mutex::new(SourceWalTransportStatus::Pending),
            credit: Mutex::new(Some(Credit {
                shared: Arc::downgrade(&self.shared),
                bytes: size,
                _handshake: handshake_credit,
            })),
        });
        q.requests.push_back(PrepareRequest {
            expected,
            correlation,
            deadline,
            ticket: Arc::clone(&ticket),
            require_binding,
            policy,
            lease,
            handshake,
        });
        Ok(SourceWalTransportRequest {
            ticket,
            transferred: false,
        })
    }
    /// Observe actual local queue/retained bytes in explicit test-support builds only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn context_usage_for_tests(&self) -> (usize, usize) {
        let q = self.shared.queue.lock().unwrap();
        (q.slots, q.bytes)
    }
    /// Inject actual retained WAL I/O failure at the original producer, never a positive fact.
    #[cfg(feature = "test-support")]
    pub fn set_unknown_wal_for_tests(&self, enabled: bool) {
        self.shared.unknown_wal.store(enabled, Ordering::Release);
    }
    /// Obtain a weak accounting probe; it retains no source or memory authority.
    #[cfg(feature = "test-support")]
    pub fn accounting_probe_for_tests(&self) -> SourceWalAccountingProbe {
        SourceWalAccountingProbe(Arc::downgrade(&self.shared))
    }
}
impl SourceWalTransportInbox {
    /// Close admission without claiming native cleanup.
    pub fn close(&self) {
        close(&self.shared);
    }
    /// Take one original bounded producer command without executing SQL or extending its deadline.
    pub fn try_next(&mut self) -> Option<SourceWalTransportCommand> {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        // Preserve audited-only API without reordering or coercing a lease subject into a revision.
        if q.requests.front()?.lease.is_some() {
            return None;
        }
        Some(SourceWalTransportCommand {
            request: q.requests.pop_front(),
            shared: Arc::clone(&self.shared),
        })
    }
    /// Take the next genuine typed command in the original FIFO, including pre-audit leases.
    pub fn try_next_any(&mut self) -> Option<SourceWalProducerCommand> {
        let request = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .pop_front()?;
        let lease = request.lease.is_some();
        let command = SourceWalTransportCommand {
            request: Some(request),
            shared: Arc::clone(&self.shared),
        };
        Some(if lease {
            SourceWalProducerCommand::Lease(SourceLeaseWalTransportCommand(command))
        } else {
            SourceWalProducerCommand::Audited(command)
        })
    }
}
impl Drop for SourceWalTransportInbox {
    fn drop(&mut self) {
        self.close();
    }
}
/// Non-Clone original producer command. Only acquiring its actual writer can publish Prepared.
///
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceWalTransportCommand;
/// let forged = SourceWalTransportCommand {};
/// ```
pub struct SourceWalTransportCommand {
    request: Option<PrepareRequest>,
    shared: Arc<Shared>,
}
impl SourceWalTransportCommand {
    fn request(&self) -> &PrepareRequest {
        self.request.as_ref().expect("original source command")
    }
    /// Original owner-produced expected revision; not an admission grant.
    pub fn expected_revision(&self) -> &SourceRevision {
        self.request().expected.as_ref().expect("pending revision")
    }
    /// Unchanged original deadline, including queue wait.
    pub fn deadline(&self) -> Instant {
        self.request().deadline
    }
    /// Caller drop requests cancellation; this does not prove rollback.
    pub fn is_cancelled(&self) -> bool {
        self.request().ticket.cancelled.load(Ordering::Acquire)
    }
    /// Reject this pending command before native writer custody was acquired.
    pub fn reject(mut self, reason: SourceWalTransportRejection) {
        self.request
            .take()
            .expect("original source command")
            .finish(SourceWalTransportStatus::Rejected(reason));
    }
    /// Move the genuine actual source writer into sealed private producer custody.
    /// This executes the original nonblocking preparation and can retain a real failed writer.
    pub fn acquire(self, writer: SourceWriter) -> SourceWalTransportOwner {
        producer::acquire(self, writer)
    }
}
impl Drop for SourceWalTransportCommand {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            request.finish(SourceWalTransportStatus::Rejected(
                SourceWalTransportRejection::OwnerLost,
            ));
        }
    }
}
/// Read-only request outcome witness for explicit test-support, not a publishing capability.
#[cfg(feature = "test-support")]
pub struct SourceWalOutcomeProbe(Arc<Ticket>);
#[cfg(feature = "test-support")]
impl SourceWalOutcomeProbe {
    /// Observe the original actor-owned state without SQL or effect authority.
    pub fn status(&self) -> SourceWalTransportStatus {
        self.0.status()
    }
}
#[cfg(feature = "test-support")]
impl SourceWalTransportRequest {
    /// Retain a read-only witness across request drop in explicit test-support builds.
    pub fn outcome_probe_for_tests(&self) -> SourceWalOutcomeProbe {
        SourceWalOutcomeProbe(Arc::clone(&self.ticket))
    }
}
/// Weak, read-only local accounting witness used only by explicit test-support fixtures.
#[cfg(feature = "test-support")]
pub struct SourceWalAccountingProbe(Weak<Shared>);
#[cfg(feature = "test-support")]
impl SourceWalAccountingProbe {
    /// Slots, bytes and closed state, or None once the real accounting object is gone.
    pub fn observe(&self) -> Option<(usize, usize, bool)> {
        let shared = self.0.upgrade()?;
        let q = shared.queue.lock().ok()?;
        Some((q.slots, q.bytes, q.closed))
    }
}
