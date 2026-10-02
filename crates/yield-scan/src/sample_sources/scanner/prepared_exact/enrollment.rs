//! Genuine scanner-prepared metadata custody through the retained source enrollment command.
use super::*;
use std::cell::RefCell;
use yield_library::operation_journal::{OperationId, StoredRelativePath, work_plan::WorkUnitKey};
use yield_library::sample_sources::db::{
    ManifestEnrollmentEntry, ManifestEnrollmentError, ManifestEnrollmentKind,
    ManifestEnrollmentRequest, ManifestEnrollmentResult, SourceWriteBatch,
};
#[cfg(test)]
mod tests;
impl PreparedExactScan<'_> {
    fn enrollment_entries(&self) -> Result<Vec<ManifestEnrollmentEntry>, ManifestEnrollmentError> {
        let mut entries = Vec::with_capacity(self.entry_count());
        for audio in &self.audio {
            entries.push(ManifestEnrollmentEntry {
                path: StoredRelativePath::new(
                    audio
                        .facts
                        .relative
                        .to_str()
                        .ok_or(ManifestEnrollmentError::Input)?,
                )
                .map_err(|_| ManifestEnrollmentError::Input)?,
                kind: ManifestEnrollmentKind::Audio,
                size: audio.facts.size,
                modified_ns: audio.facts.modified_ns,
                file_identity: audio
                    .facts
                    .file_identity
                    .clone()
                    .ok_or(ManifestEnrollmentError::Input)?,
                content_hash: Some(audio.hash.clone()),
            });
        }
        for index in &self.index {
            entries.push(ManifestEnrollmentEntry {
                path: StoredRelativePath::new(
                    index
                        .facts
                        .relative
                        .to_str()
                        .ok_or(ManifestEnrollmentError::Input)?,
                )
                .map_err(|_| ManifestEnrollmentError::Input)?,
                kind: ManifestEnrollmentKind::Index {
                    classification: index.entry.classification,
                    diagnostic: index.entry.diagnostic,
                    policy: index.entry.format_policy_version,
                },
                size: index.facts.size,
                modified_ns: index.facts.modified_ns,
                file_identity: index
                    .facts
                    .file_identity
                    .clone()
                    .ok_or(ManifestEnrollmentError::Input)?,
                content_hash: None,
            });
        }
        entries.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        Ok(entries)
    }
    /// Capture canonical proposal from these genuine held files before dispatch.
    /// The admitted family supplies its frozen key; no ordinal or physical grant is invented.
    /// Enrollment v1 accepts UTF-8 exact keys. Raw locators retain the ordinary index scan route.
    pub fn enrollment_request(
        &self,
        operation: OperationId,
        work: WorkUnitKey,
    ) -> Result<ManifestEnrollmentRequest, ManifestEnrollmentError> {
        self.owner
            .validate_scan_fence(&self.fence)
            .map_err(ManifestEnrollmentError::Owner)?;
        ManifestEnrollmentRequest::new(operation, work, self.owner, self.enrollment_entries()?)
    }
    /// Revalidate these exact held files and commit metadata plus receipt in one source TX.
    /// For lost acknowledgement/restart, first call SourceWriter::replay_manifest_enrollment
    /// with restored original custody, before attempting any new filesystem preparation.
    pub fn commit_enrollment(
        self,
        request: &ManifestEnrollmentRequest,
        cancel: Option<&AtomicBool>,
    ) -> Result<ManifestEnrollmentResult, ScanError> {
        if self.fence != *request.current_fence()
            || self.enrollment_entries().map_err(enrollment_error)? != request.entries()
        {
            return Err(enrollment_error(ManifestEnrollmentError::Input));
        }
        let writer = EnrollmentWriter {
            owner: self.owner,
            request,
            cancel,
            result: RefCell::new(None),
        };
        self.commit(&writer, cancel)?;
        writer
            .result
            .into_inner()
            .ok_or_else(|| enrollment_error(ManifestEnrollmentError::Evidence))
    }
}
struct EnrollmentWriter<'a> {
    owner: &'a SourceWriter,
    request: &'a ManifestEnrollmentRequest,
    cancel: Option<&'a AtomicBool>,
    result: RefCell<Option<ManifestEnrollmentResult>>,
}
impl ScanWriter for EnrollmentWriter<'_> {
    type Guard = ();
    fn lock(&self, phase: ScanWritePhase) {
        self.owner.lock(phase);
    }
    fn try_lock(&self, phase: ScanWritePhase) -> Result<(), ScanError> {
        self.owner.try_lock(phase)
    }
    fn commit_bounded_manifest_batch(
        &self,
        batch: SourceWriteBatch<'_>,
        expected: u64,
    ) -> Result<ManifestCommitResult, ScanError> {
        if expected != self.request.current_fence().revision() {
            return Err(enrollment_error(ManifestEnrollmentError::Input));
        }
        let result = self
            .owner
            .commit_manifest_enrollment(self.request, batch, self.cancel)
            .map_err(enrollment_error)?;
        let delta = result.receipt.manifest().clone();
        self.result.replace(Some(result));
        Ok(delta)
    }
}
fn enrollment_error(e: ManifestEnrollmentError) -> ScanError {
    match e {
        ManifestEnrollmentError::Canceled => ScanError::Canceled,
        other => ScanError::SourceOwner(other.to_string()),
    }
}
