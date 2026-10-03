//! Pre-intent catalog custody only. No control here can acknowledge journal settlement.
use super::*;
use std::sync::Weak;
use yield_library::operation_journal::{AdmissionClaimId, ParticipantId};
use yield_library::sample_sources::SourceRevision;
use yield_library::sample_sources::db::write::{SourceWalPreparation, SourceWalPreparationFailure};
mod owner;
pub(super) use owner::{WorkerReservations, deferred, interrupts_preparation, lifecycle};

/// Maximum pending plus held source preparation slots for one catalog worker.
pub const CATALOG_WAL_MAX_SLOTS: usize = 64;
/// Aggregate retained handshake identity/correlation bytes, independently of normal commands.
pub const CATALOG_WAL_MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_DEADLINE: Duration = Duration::from_secs(2);
// Charge complete retained payloads plus their concrete headers/observations and bounded
// owner bookkeeping. The original SourceWriter/database, snapshots and watcher already
// existed in RegisteredSource and move without duplication. Empty queue backing stores
// no handshake payload; its separate allocation is bounded by the same 64-slot limit.
const CONTEXT_FIXED_BYTES: usize = std::mem::size_of::<PrepareRequest>()
    + std::mem::size_of::<Ticket>()
    + std::mem::size_of::<Credit>()
    + std::mem::size_of::<SourceWalPreparation>()
        .saturating_sub(std::mem::size_of::<SourceWriter>())
    + 128; // Arc headers, slot deadline/cancellation state, nonce spelling and map bookkeeping.

/// Bounded immutable correlations; these IDs do not issue or reserve any capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSourceWalCorrelation {
    command_digest: [u8; 32],
    participant: ParticipantId,
    database_claim: AdmissionClaimId,
    sidecar_claim: AdmissionClaimId,
}
impl CatalogSourceWalCorrelation {
    /// Bind one command digest and distinct nonnil DB and WAL/SHM claim observations.
    pub fn new(
        command_digest: [u8; 32],
        participant: ParticipantId,
        database_claim: AdmissionClaimId,
        sidecar_claim: AdmissionClaimId,
    ) -> Result<Self, CatalogSourceWalRejection> {
        if command_digest == [0; 32]
            || participant.as_uuid().is_nil()
            || database_claim.validate().is_err()
            || sidecar_claim.validate().is_err()
            || database_claim == sidecar_claim
        {
            return Err(CatalogSourceWalRejection::InvalidContext);
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
pub enum CatalogSourceWalRejection {
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSourceWalObservation {
    revision: SourceRevision,
    correlation: CatalogSourceWalCorrelation,
    nonce: String,
    wal_bytes: u64,
}
impl CatalogSourceWalObservation {
    /// Complete source revision observed by the original retained writer.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Retained VFS WAL byte observation, not a current watermark permission.
    pub fn wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Original command/participant/claim observations, not allocated claims.
    pub fn correlation(&self) -> &CatalogSourceWalCorrelation {
        &self.correlation
    }
    /// Opaque local preparation nonce, not an OperationId.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
}

/// Observed owner-controlled state. Caller clocks and handle drop never confirm rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogSourceWalStatus {
    /// Bounded control was admitted; no SQLite reservation is implied.
    Pending,
    /// Actual writer is parked under the original live pre-intent reservation.
    Prepared(CatalogSourceWalObservation),
    /// Abort has been requested; the writer is still exclusively retained.
    Aborting,
    /// The original writer was returned only after confirmed rollback and timeout restoration.
    Aborted,
    /// The original draining source authority confirmed connection and lease retirement.
    /// Journal finalization may still be pending; this does not mean a reusable writer exists.
    Retired,
    /// Rejection is confirmed without a retained preparation resource.
    Rejected(CatalogSourceWalRejection),
    /// Cleanup remains unresolved; the private writer is unavailable for ordinary SQL.
    Unavailable(CatalogSourceWalRejection),
}
struct Ticket {
    cancelled: AtomicBool,
    status: Mutex<CatalogSourceWalStatus>,
    credit: Mutex<Option<Credit>>,
}
impl Ticket {
    fn status(&self) -> CatalogSourceWalStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn set(&self, status: CatalogSourceWalStatus) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    }
    fn finish(&self, status: CatalogSourceWalStatus) {
        // Owner calls this only after a returned writer or confirmed absence of reservation.
        drop(self.credit.lock().unwrap_or_else(|p| p.into_inner()).take());
        self.set(status);
    }
}

/// Non-clone pending control holder. Drop signals abort, retaining owner-side custody.
pub struct CatalogSourceWalRequest {
    ticket: Arc<Ticket>,
    transferred: bool,
}
impl CatalogSourceWalRequest {
    /// Read bounded state without SQL, lease validation or timer-driven release.
    pub fn status(&self) -> CatalogSourceWalStatus {
        self.ticket.status()
    }
    /// Transfer the sole cancellation holder after the real worker publishes preparation.
    /// A copied observation cannot perform this transfer or acquire any authority.
    pub fn try_prepared(&mut self) -> Option<CatalogSourceWalPreparation> {
        if self.transferred || !matches!(self.ticket.status(), CatalogSourceWalStatus::Prepared(_))
        {
            return None;
        }
        self.transferred = true;
        Some(CatalogSourceWalPreparation {
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
impl Drop for CatalogSourceWalRequest {
    fn drop(&mut self) {
        if !self.transferred {
            self.cancel();
        }
    }
}
/// Non-clone delivered pre-intent holder, containing no SQLite writer, lease or effect permission.
pub struct CatalogSourceWalPreparation {
    ticket: Arc<Ticket>,
}
impl CatalogSourceWalPreparation {
    /// Observe prepared/aborting/terminal custody; only the original worker changes outcomes.
    pub fn status(&self) -> CatalogSourceWalStatus {
        self.ticket.status()
    }
    /// Ask the original owner to abort; this does not synchronously unlock or return a writer.
    pub fn abort(&self) {
        self.ticket.cancelled.store(true, Ordering::Release);
    }
    /// Explicitly reject any attempt to use a pre-intent holder as a post-latch decision.
    pub fn acknowledge_settlement(&self) -> Result<(), CatalogSourceWalRejection> {
        Err(CatalogSourceWalRejection::SettlementUnsupported)
    }
}
impl Drop for CatalogSourceWalPreparation {
    fn drop(&mut self) {
        self.abort();
    }
}

struct PrepareRequest {
    expected: SourceRevision,
    correlation: CatalogSourceWalCorrelation,
    deadline: Instant,
    ticket: Arc<Ticket>,
}
struct Queue {
    requests: VecDeque<PrepareRequest>,
    slots: usize,
    bytes: usize,
    closed: bool,
}
#[derive(Clone)]
pub(super) struct Control {
    shared: Arc<Shared>,
}
struct Shared {
    queue: Mutex<Queue>,
    #[cfg(test)]
    unknown_wal: AtomicBool,
}
struct Credit {
    shared: Weak<Shared>,
    bytes: usize,
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
impl Control {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    requests: VecDeque::new(),
                    slots: 0,
                    bytes: 0,
                    closed: false,
                }),
                #[cfg(test)]
                unknown_wal: AtomicBool::new(false),
            }),
        }
    }
    fn submit(
        &self,
        expected: SourceRevision,
        correlation: CatalogSourceWalCorrelation,
        deadline: Instant,
    ) -> Result<CatalogSourceWalRequest, CatalogSourceWalRejection> {
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > MAX_DEADLINE {
            return Err(CatalogSourceWalRejection::Deadline);
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
        .ok_or(CatalogSourceWalRejection::InvalidContext)?;
        if size > CATALOG_WAL_MAX_CONTEXT_BYTES {
            return Err(CatalogSourceWalRejection::InvalidContext);
        }
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if q.closed {
            return Err(CatalogSourceWalRejection::Closed);
        }
        if q.slots >= CATALOG_WAL_MAX_SLOTS || q.bytes > CATALOG_WAL_MAX_CONTEXT_BYTES - size {
            return Err(CatalogSourceWalRejection::Full);
        }
        q.slots += 1;
        q.bytes += size;
        let ticket = Arc::new(Ticket {
            cancelled: AtomicBool::new(false),
            status: Mutex::new(CatalogSourceWalStatus::Pending),
            credit: Mutex::new(Some(Credit {
                shared: Arc::downgrade(&self.shared),
                bytes: size,
            })),
        });
        q.requests.push_back(PrepareRequest {
            expected,
            correlation,
            deadline,
            ticket: Arc::clone(&ticket),
        });
        Ok(CatalogSourceWalRequest {
            ticket,
            transferred: false,
        })
    }
    pub(super) fn close(&self) {
        let pending = {
            let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.closed = true;
            q.requests.drain(..).collect::<Vec<_>>()
        };
        for request in pending {
            request.ticket.finish(CatalogSourceWalStatus::Rejected(
                CatalogSourceWalRejection::Closed,
            ));
        }
    }
    fn take(&self) -> Option<PrepareRequest> {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .pop_front()
    }
}
impl CatalogService {
    /// Queue genuine retained-source pre-intent preparation on the original catalog worker.
    /// Expected revision is owner-produced and nonserializable. Correlations are observations;
    /// this operation never issues an ordinary ID, physical claim, journal latch or write grant.
    pub fn prepare_source_wal(
        &self,
        expected: SourceRevision,
        correlation: CatalogSourceWalCorrelation,
        deadline: Instant,
    ) -> Result<CatalogSourceWalRequest, CatalogSourceWalRejection> {
        let state = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutdown || state.closed {
            return Err(CatalogSourceWalRejection::Closed);
        }
        let desired = state
            .sources
            .get(expected.source_id())
            .ok_or(CatalogSourceWalRejection::Scope)?;
        if !desired.active || desired.lifecycle_generation != expected.lifecycle_generation() {
            return Err(CatalogSourceWalRejection::Scope);
        }
        state.source_wal.submit(expected, correlation, deadline)
    }
}

#[cfg(test)]
mod tests;
