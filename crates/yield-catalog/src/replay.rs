//! Bounded production replay requests. Native envelopes remain observations until owner admission.
use super::*;
use std::sync::atomic::AtomicUsize;
use yield_library::sample_sources::db::{DurableReplayReceipt, ManifestCommitResult};
use yield_source_watcher::replay::{
    CapturedNativeReplayBatch, MAX_REPLAY_EVENTS, MAX_REPLAY_PATH_BYTES, ReplayObservationBatch,
};

mod recovery;
mod state;
#[cfg(test)]
mod tests;
mod worker;
mod writer;
pub(super) use state::ReplayState;

/// Global bound covering queued, retained-failure and caller-held replay requests.
pub const MAX_CATALOG_REPLAY_REQUESTS: usize = 8;
/// Maximum manifest rows in one complete bounded replay transaction.
pub const MAX_CATALOG_REPLAY_DELTA_ENTRIES: usize =
    yield_scan::sample_sources::scanner::EXACT_SCAN_MAX_ENTRIES;
/// Passive committed manifest/index/directory delta for renderer projection; no writer access.
pub type CatalogReplayDelta = ManifestCommitResult;
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug)]
struct Permit;
impl Permit {
    fn acquire() -> Result<Arc<Self>, CatalogReplaySubmitError> {
        REQUESTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CATALOG_REPLAY_REQUESTS).then_some(n + 1)
            })
            .map_err(|_| CatalogReplaySubmitError::Busy)?;
        Ok(Arc::new(Self))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        REQUESTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Fixed replay failure vocabulary; failures preserve last-good source facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CatalogReplayError {
    /// The registered physical source/lifecycle no longer matches.
    #[error("stale replay source lifecycle")]
    StaleLifecycle,
    /// The source revision differs from the request's observed revision.
    #[error("stale replay source revision")]
    StaleRevision,
    /// Caller or source-owner cancellation was observed before mutation.
    #[error("replay canceled")]
    Canceled,
    /// Profile, database or physical source owner is unavailable.
    #[error("replay owner unavailable")]
    OwnerUnavailable,
    /// No validated durable prior is available; audit/bootstrap is required.
    #[error("durable replay prior unavailable")]
    MissingPrior,
    /// Root, stream or watcher generation differs from the retained owner.
    #[error("replay identity mismatch")]
    WrongIdentity,
    /// Native cursor coverage is missing, ambiguous, gapped or overflowed.
    #[error("replay coverage requires audit")]
    GapOrOverflow,
    /// Directory/unknown scope requires conservative authoritative audit.
    #[error("replay subtree requires authoritative audit")]
    SubtreeAudit,
    /// Exact regular target is unknown, changing, unavailable or oversized.
    #[error("replay exact target requires audit")]
    InvalidTarget,
    /// A durable commit outcome could not be confirmed; raw work remains retained.
    #[error("replay commit outcome uncertain")]
    CommitUncertain,
    /// Another unacknowledged batch still owns this source's bounded slot.
    #[error("different replay batch remains pending")]
    PendingDifferent,
}

/// Rejected nonblocking request admission; no source mutation occurred.
#[derive(Debug, thiserror::Error)]
pub enum CatalogReplaySubmitError {
    /// The fixed global request/retention bound is full.
    #[error("catalog replay request bound reached")]
    Busy,
    /// Envelope or identity bytes exceed the producer/owner limits.
    #[error("catalog replay input exceeds bounds")]
    InputTooLarge,
    /// Existing catalog command admission failed.
    #[error(transparent)]
    Catalog(#[from] CatalogSubmitError),
}

/// Actual worker outcome, including bounded delta and opaque durable acknowledgement.
pub enum CatalogReplayOutcome {
    /// The exact manifest/index delta and checkpoint committed in one transaction.
    Committed {
        /// Database-issued persisted receipt; no native qualification is implied.
        receipt: DurableReplayReceipt,
        /// At most the scanner's bounded touched rows; no whole-source snapshot fallback.
        delta: ManifestCommitResult,
    },
    /// Exact interval/terminal was already durably covered; no new delta is fabricated.
    PreviouslyCommitted {
        /// Read-back persisted authority after duplicate delivery/restart/lost response.
        receipt: DurableReplayReceipt,
    },
    /// No replay acknowledgement; conservative source audit remains required.
    AuditRequired {
        /// Typed retained failure.
        reason: CatalogReplayError,
    },
    /// No acknowledgement or partial success; retain/retry the bounded request.
    Retained {
        /// Typed denial or uncertain outcome.
        reason: CatalogReplayError,
        /// Original bounded producer envelope returned for retry, without an acknowledgement.
        batch: ReplayObservationBatch,
    },
}

/// Caller-held bounded result and denial token, never a continuity/physical grant.
pub struct CatalogReplayRequest {
    reply: Receiver<CatalogReplayOutcome>,
    cancellation: Arc<AtomicBool>,
    // Retain the original native envelope through caller-held reply/cancellation.
    _capture: Option<Arc<CapturedNativeReplayBatch>>,
    _permit: Arc<Permit>,
}
impl CatalogReplayRequest {
    /// Cancel queued/prepared work. A confirmed durable commit cannot be undone by cancellation.
    pub fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }
    /// Read a ready outcome without blocking. Dropped/closed workers report Closed.
    pub fn try_recv(&self) -> Result<Option<CatalogReplayOutcome>, CatalogReplaySubmitError> {
        match self.reply.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(CatalogSubmitError::Closed.into()),
        }
    }
}
impl Drop for CatalogReplayRequest {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Opaque command payload minted only by the bounded service request API.
#[derive(Debug)]
pub struct CatalogReplaySubmission {
    batch: ReplayObservationBatch,
    lifecycle_generation: u64,
    expected_revision: u64,
    cancellation: Arc<AtomicBool>,
    reply: mpsc::SyncSender<CatalogReplayOutcome>,
    // The actual worker retains the same original capture even after a lost caller reply.
    _capture: Option<Arc<CapturedNativeReplayBatch>>,
    permit: Arc<Permit>,
}
impl CatalogReplaySubmission {
    pub(super) fn source_id(&self) -> &SourceId {
        self.batch.batch().provenance().source_id()
    }
    pub(super) fn input_bytes(&self) -> usize {
        input_bytes(&self.batch)
    }
}
fn input_bytes(batch: &ReplayObservationBatch) -> usize {
    let raw = batch.batch();
    let provenance = raw.provenance();
    provenance
        .source_id()
        .as_str()
        .len()
        .saturating_add(provenance.root_identity().map_or(0, |r| r.as_bytes().len()))
        .saturating_add(
            provenance
                .backend_stream_identity()
                .map_or(0, |s| s.as_bytes().len()),
        )
        .saturating_add(
            raw.observations()
                .iter()
                .flat_map(|o| o.paths())
                .fold(0usize, |n, p| {
                    n.saturating_add(p.path().as_os_str().as_encoded_bytes().len())
                }),
        )
        .saturating_add(raw.observations().len().saturating_mul(128))
}

impl CatalogService {
    /// Submit one genuine producer envelope to the actual retained catalog worker.
    /// Identity, durable-prior/proof, revision, lifecycle and exact target checks occur there;
    /// a pathless watcher hint cannot construct this opaque payload or advance a checkpoint.
    pub fn submit_replay(
        &self,
        batch: ReplayObservationBatch,
        lifecycle_generation: u64,
        expected_revision: u64,
    ) -> Result<CatalogReplayRequest, CatalogReplaySubmitError> {
        self.submit_replay_with_capture(batch, lifecycle_generation, expected_revision, None, None)
    }

    /// Queue an actual native capture without releasing its original backing at admission.
    /// The existing worker still requires genuine durable prior, lifecycle/root/revision
    /// and exact Source admission. This wrapper is not a proof or committed acknowledgement.
    /// Keep an original `Arc` when submitting a clone: admission denial leaves that
    /// original available for retry, and dropping a reply cannot release worker custody.
    pub fn submit_captured_replay(
        &self,
        captured: Arc<CapturedNativeReplayBatch>,
        lifecycle_generation: u64,
        expected_revision: u64,
    ) -> Result<CatalogReplayRequest, CatalogReplaySubmitError> {
        // Acquire the existing consumer's bounded slot before making its own input copy.
        // The original capture's slot remains held by the retained opaque wrapper.
        let permit = Permit::acquire()?;
        let batch = captured.observation().clone();
        self.submit_replay_with_capture(
            batch,
            lifecycle_generation,
            expected_revision,
            Some(captured),
            Some(permit),
        )
    }

    fn submit_replay_with_capture(
        &self,
        batch: ReplayObservationBatch,
        lifecycle_generation: u64,
        expected_revision: u64,
        capture: Option<Arc<CapturedNativeReplayBatch>>,
        permit: Option<Arc<Permit>>,
    ) -> Result<CatalogReplayRequest, CatalogReplaySubmitError> {
        if batch.batch().observations().len() > MAX_REPLAY_EVENTS + 1
            || input_bytes(&batch) > MAX_REPLAY_PATH_BYTES + (MAX_REPLAY_EVENTS + 1) * 128 + 3072
        {
            return Err(CatalogReplaySubmitError::InputTooLarge);
        }
        let permit = match permit {
            Some(permit) => permit,
            None => Permit::acquire()?,
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = mpsc::sync_channel(1);
        self.submit(CatalogCommand::ConsumeReplay {
            submission: CatalogReplaySubmission {
                batch,
                lifecycle_generation,
                expected_revision,
                cancellation: cancellation.clone(),
                reply,
                _capture: capture.clone(),
                permit: permit.clone(),
            },
        })?;
        Ok(CatalogReplayRequest {
            reply: receiver,
            cancellation,
            _capture: capture,
            _permit: permit,
        })
    }
}

pub(super) fn consume(
    submission: CatalogReplaySubmission,
    profile: &WritableProfileGuard,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let outcome = worker::consume(&submission, profile, admission, sources);
    let _ = submission.reply.try_send(outcome);
}

/// The existing retained catalog thread consumes its own native result; no second SQL owner.
pub(super) fn consume_captured_on_worker(
    capture: Arc<CapturedNativeReplayBatch>,
    lifecycle_generation: u64,
    expected_revision: u64,
    profile: &WritableProfileGuard,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> Result<CatalogReplayOutcome, CatalogReplaySubmitError> {
    let source_id = capture.observation().batch().provenance().source_id();
    let permit = match sources
        .get(source_id)
        .and_then(|s| s.replay.as_ref())
        .and_then(|state| state.capture_permit(&capture))
    {
        Some(permit) => permit,
        None => Permit::acquire()?,
    };
    let (reply, _unused_receiver) = mpsc::sync_channel(1);
    let submission = CatalogReplaySubmission {
        batch: capture.observation().clone(),
        lifecycle_generation,
        expected_revision,
        cancellation: Arc::new(AtomicBool::new(false)),
        reply,
        _capture: Some(capture),
        permit,
    };
    Ok(worker::consume(&submission, profile, admission, sources))
}
