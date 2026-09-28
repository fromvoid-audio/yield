use super::scan::ScanError;
use yield_library::sample_sources::SourceManifestEntry;
use yield_library::sample_sources::db::{ManifestCommitResult, SourceWriteBatch};
use yield_library::sample_sources::reconciliation::{
    RootIdentity, SourceAuditCommit, SourceAuditRequest,
};
use yield_library::sample_sources::{SourceWriter, SourceWriterError};

/// Database mutation phases exposed by the scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanWritePhase {
    /// Open or migrate the source database before scanning.
    Open,
    /// Commit an indexed manifest batch or scan-completion checkpoint.
    Manifest,
    /// Publish deferred hashes and proven rename reconciliation.
    DeferredHash,
}

/// Coordinates scanner database writes with an owning runtime.
///
/// The returned guard must retain exclusive write ownership until it is dropped. Filesystem
/// traversal, file reads, and content hashing deliberately happen before `lock` is called.
pub trait ScanWriter {
    /// Guard retaining write ownership for one bounded mutation.
    type Guard;

    /// Acquire write ownership for the supplied scanner phase.
    fn lock(&self, phase: ScanWritePhase) -> Self::Guard;

    /// Try to acquire write ownership for one bounded scanner mutation.
    ///
    /// Existing coordinators retain the infallible `lock` contract by default. Owners that must
    /// revalidate profile or source authority before each write phase can override this method
    /// and fail closed without changing existing scanner callers.
    fn try_lock(&self, phase: ScanWritePhase) -> Result<Self::Guard, ScanError> {
        Ok(self.lock(phase))
    }

    /// Acquire ownership for discarding private staging rows after an aborted audit.
    ///
    /// Cleanup must still validate the owning profile/source boundary, but it deliberately does
    /// not inherit an operation's cancellation flag. The caller only uses this for rows created
    /// by the failed audit and never for active or inactive directory generations.
    fn try_lock_cleanup(&self) -> Result<Self::Guard, ScanError> {
        self.try_lock(ScanWritePhase::Manifest)
    }

    /// Commit a prepared, revision-fenced manifest batch through the active
    /// source owner. Callers must hold this writer's bounded mutation guard.
    fn commit_bounded_manifest_batch(
        &self,
        batch: SourceWriteBatch<'_>,
        expected_revision: u64,
    ) -> Result<ManifestCommitResult, ScanError> {
        Ok(batch.commit_with_bounded_manifest_changes(expected_revision)?)
    }

    /// Activate a complete staged audit and return the committed manifest on
    /// the same owner that admitted its bounded scan batches.
    fn commit_scan_audit_completion(
        &self,
        batch: SourceWriteBatch<'_>,
        generation: u64,
        expected_revision: u64,
        completed_at: i64,
        root_identity: RootIdentity,
        audit_request: Option<&SourceAuditRequest>,
    ) -> Result<(SourceAuditCommit, Vec<SourceManifestEntry>), ScanError> {
        Ok(batch.commit_manifest_audit_with_staged_directory_truth(
            generation,
            expected_revision,
            completed_at,
            root_identity,
            audit_request,
        )?)
    }
}

/// Default coordinator for callers that already own serialization or only need SQLite's
/// per-database transaction boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct UncoordinatedScanWriter;

impl ScanWriter for UncoordinatedScanWriter {
    type Guard = ();

    fn lock(&self, _phase: ScanWritePhase) -> Self::Guard {}
}

/// A retained source owner can coordinate targeted scanner writes directly.
/// The catalog's own scan writer adds lifecycle admission on top of this
/// physical source check when a catalog operation owns the scan.
impl ScanWriter for SourceWriter {
    type Guard = ();

    fn lock(&self, phase: ScanWritePhase) -> Self::Guard {
        self.try_lock(phase)
            .expect("source scan writer lock must retain physical authority")
    }

    fn try_lock(&self, _phase: ScanWritePhase) -> Result<Self::Guard, ScanError> {
        self.current_scan_fence().map_err(source_writer_error)?;
        Ok(())
    }

    fn commit_bounded_manifest_batch(
        &self,
        batch: SourceWriteBatch<'_>,
        expected_revision: u64,
    ) -> Result<ManifestCommitResult, ScanError> {
        let fence = self.current_scan_fence().map_err(source_writer_error)?;
        if fence.revision() != expected_revision {
            return Err(ScanError::StaleRevision {
                expected: expected_revision,
                actual: fence.revision(),
            });
        }
        self.commit_scan_manifest_batch(&fence, batch)
            .map_err(source_writer_error)
    }

    fn commit_scan_audit_completion(
        &self,
        batch: SourceWriteBatch<'_>,
        generation: u64,
        expected_revision: u64,
        completed_at: i64,
        root_identity: RootIdentity,
        audit_request: Option<&SourceAuditRequest>,
    ) -> Result<(SourceAuditCommit, Vec<SourceManifestEntry>), ScanError> {
        let fence = self.current_scan_fence().map_err(source_writer_error)?;
        if fence.revision() != expected_revision {
            return Err(ScanError::StaleRevision {
                expected: expected_revision,
                actual: fence.revision(),
            });
        }
        self.commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            completed_at,
            root_identity,
            audit_request,
        )
        .map_err(source_writer_error)
    }
}

fn source_writer_error(error: SourceWriterError) -> ScanError {
    match error {
        SourceWriterError::Database(error) => ScanError::Db(error),
        SourceWriterError::StaleRevision { expected, actual } => {
            ScanError::StaleRevision { expected, actual }
        }
        error => ScanError::SourceOwner(error.to_string()),
    }
}
