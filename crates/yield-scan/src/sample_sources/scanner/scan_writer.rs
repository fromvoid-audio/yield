use super::scan::ScanError;

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
}

/// Default coordinator for callers that already own serialization or only need SQLite's
/// per-database transaction boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct UncoordinatedScanWriter;

impl ScanWriter for UncoordinatedScanWriter {
    type Guard = ();

    fn lock(&self, _phase: ScanWritePhase) -> Self::Guard {}
}
