//! Attach separately admitted proof/prior in the single source transaction.
use super::*;
use std::cell::{Cell, RefCell};
use yield_library::sample_sources::{
    db::SourceWriteBatch,
    reconciliation::{ReplayPriorToken, WatcherContinuityProof},
};

pub(super) struct ReplayWriter<'a> {
    pub(super) base: CatalogScanWriter<'a>,
    pub(super) cancellation: &'a AtomicBool,
    pub(super) prior: &'a ReplayPriorToken,
    pub(super) proof: &'a WatcherContinuityProof,
    pub(super) checkpoint: RefCell<Option<ReplayPriorToken>>,
    pub(super) commit_uncertain: Cell<bool>,
}
impl<'a> ScanWriter for ReplayWriter<'a> {
    type Guard = Option<MutexGuard<'a, AdmissionState>>;
    fn lock(&self, phase: ScanWritePhase) -> Self::Guard {
        self.try_lock(phase).expect("current replay owner")
    }
    fn try_lock(&self, phase: ScanWritePhase) -> Result<Self::Guard, ScanError> {
        if self.cancellation.load(Ordering::Acquire) {
            return Err(ScanError::Canceled);
        }
        let guard = self.base.try_lock(phase)?;
        if self.cancellation.load(Ordering::Acquire) {
            return Err(ScanError::Canceled);
        }
        Ok(guard)
    }
    fn commit_bounded_manifest_batch(
        &self,
        batch: SourceWriteBatch<'_>,
        expected_revision: u64,
    ) -> Result<ManifestCommitResult, ScanError> {
        if self.cancellation.load(Ordering::Acquire) {
            return Err(ScanError::Canceled);
        }
        let owner = self.base.owner.expect("retained replay owner");
        let fence = owner.current_scan_fence().map_err(scan_writer_error)?;
        if fence.revision() != expected_revision {
            return Err(ScanError::StaleRevision {
                expected: expected_revision,
                actual: fence.revision(),
            });
        }
        let (delta, checkpoint) = owner
            .commit_watcher_replay_manifest_batch(&fence, batch, self.prior, self.proof)
            .map_err(|error| {
                if matches!(
                    error,
                    SourceWriterError::ReplayCheckpointOutcomeUncertain { .. }
                ) {
                    self.commit_uncertain.set(true);
                }
                scan_writer_error(error)
            })?;
        self.checkpoint.replace(Some(checkpoint));
        Ok(delta)
    }
}
