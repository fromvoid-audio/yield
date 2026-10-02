//! One retained replay lane and causal audit recovery per registered source.
use super::*;
use yield_library::sample_sources::reconciliation::{
    DispatchedObservation, RawObservationLimits, ReconciliationAdmissionLimits,
    ReconciliationAdmissionOwner, ReconciliationAdmissionSupervisor, ReplayPriorToken,
    RetainedUncertainty, SourceAuditCommit, SourceAuditRequest,
};

pub(super) struct PendingReplay {
    pub(super) batch: ReplayObservationBatch,
    pub(super) dispatched: DispatchedObservation,
    pub(super) applied: bool,
}
pub(crate) struct ReplayState {
    pub(super) owner: ReconciliationAdmissionOwner,
    pub(super) pending: Option<PendingReplay>,
    pub(crate) audit_request: Option<SourceAuditRequest>,
    pub(super) needs_audit: bool,
    pub(super) audit_batch: Option<ReplayObservationBatch>,
    _permit: Arc<Permit>,
}
impl ReplayState {
    pub(super) fn new(
        source: SourceId,
        root: RootIdentity,
        prior: Option<&ReplayPriorToken>,
        permit: Arc<Permit>,
    ) -> Result<Self, CatalogReplayError> {
        let raw =
            RawObservationLimits::new(MAX_REPLAY_EVENTS + 1, MAX_REPLAY_PATH_BYTES, 64 * 1024)
                .expect("fixed bounds");
        let global = RawObservationLimits::new(
            (MAX_REPLAY_EVENTS + 1) * 2,
            MAX_REPLAY_PATH_BYTES * 2,
            128 * 1024,
        )
        .expect("fixed bounds");
        let limits =
            ReconciliationAdmissionLimits::new(1, raw, global, 2, 8, 4).expect("fixed lane bounds");
        let mut owner =
            ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
        if let Some(prior) = prior {
            owner
                .restore_replay_source(prior)
                .map_err(|_| CatalogReplayError::WrongIdentity)?;
        } else {
            owner
                .begin_source(source, root)
                .map_err(|_| CatalogReplayError::OwnerUnavailable)?;
        }
        Ok(Self {
            owner,
            pending: None,
            audit_request: None,
            needs_audit: false,
            audit_batch: None,
            _permit: permit,
        })
    }
    pub(super) fn require_audit(
        &mut self,
        batch: &ReplayObservationBatch,
    ) -> Option<RetainedUncertainty> {
        self.needs_audit = true;
        self.audit_batch.get_or_insert_with(|| batch.clone());
        let admitted = self
            .owner
            .admit_replay_with_durable_prior(batch.batch().clone(), None, false)
            .ok()?;
        let source = batch.batch().provenance().source_id();
        self.audit_request = admitted
            .audit_request()
            .cloned()
            .or_else(|| self.owner.source_audit_request_for_current_lane(source));
        self.owner.current_durable_audit_marker(source)
    }
    pub(crate) fn acknowledge_audit(&mut self, commit: &SourceAuditCommit) -> bool {
        let Some(request) = &self.audit_request else {
            return false;
        };
        let Some(receipt) = request.complete_from_committed_audit(commit.clone()) else {
            return false;
        };
        let outcome = self.owner.acknowledge_source_audit_receipt(&receipt);
        outcome.cleared_markers() > 0 && outcome.remaining_markers() == 0
    }
}
