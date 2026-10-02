//! Same-transaction source metadata delta and immutable receipt publication.
use super::*;
impl SourceWriter {
    fn issue_manifest_receipt(
        &self,
        r: &ManifestEnrollmentRequest,
        result: ManifestCommitResult,
    ) -> Result<ManifestEnrollmentReceipt, ManifestEnrollmentError> {
        let receipt = ManifestEnrollmentReceipt {
            payload: r.payload.clone(),
            bytes: r.bytes.clone(),
            result,
            publication: self.database.source_publication_fence()?,
            owner: Arc::downgrade(&self.reference_liveness),
        };
        receipt.validate()?;
        Ok(receipt)
    }
    /// Recover an immutable acknowledgement before re-preparing files or opening a mutation.
    /// Fresh actual ownership is required, but historical generation/revision remain immutable.
    pub fn replay_manifest_enrollment(
        &self,
        r: &ManifestEnrollmentRequest,
    ) -> Result<Option<ManifestEnrollmentResult>, ManifestEnrollmentError> {
        if !Weak::ptr_eq(&r.owner, &Arc::downgrade(&self.reference_liveness)) {
            return Err(ManifestEnrollmentError::OwnerEnded);
        }
        self.validate_scan_fence(&r.current)?;
        if !r
            .payload
            .source
            .same_storage(&Scope::from_fence(&r.current))
        {
            return Err(ManifestEnrollmentError::Input);
        }
        let result = ledger::read(&self.database.connection, r)?;
        self.validate_scan_fence(&r.current)?;
        result
            .map(|m| {
                Ok(ManifestEnrollmentResult {
                    receipt: self.issue_manifest_receipt(r, m)?,
                    replayed: true,
                })
            })
            .transpose()
    }
    /// Atomically commit one exact prepared batch and immutable operation/work receipt.
    /// Filesystem preparation belongs outside SQL; the scanner owns revalidation/cancellation.
    pub fn commit_manifest_enrollment(
        &self,
        r: &ManifestEnrollmentRequest,
        batch: SourceWriteBatch<'_>,
        cancel: Option<&AtomicBool>,
    ) -> Result<ManifestEnrollmentResult, ManifestEnrollmentError> {
        if !std::ptr::eq(batch.owner, &self.database) {
            return Err(ManifestEnrollmentError::Input);
        }
        if !Weak::ptr_eq(&r.owner, &Arc::downgrade(&self.reference_liveness)) {
            return Err(ManifestEnrollmentError::OwnerEnded);
        }
        self.validate_scan_fence(&r.current)?;
        canceled(cancel)?;
        if let Some(result) = ledger::read(&batch.tx, r)? {
            batch.tx.rollback()?;
            self.validate_scan_fence(&r.current)?;
            return Ok(ManifestEnrollmentResult {
                receipt: self.issue_manifest_receipt(r, result)?,
                replayed: true,
            });
        }
        if r.payload.source != Scope::from_fence(&r.current) {
            return Err(ManifestEnrollmentError::Input);
        }
        ledger::capacity(&batch.tx, r.bytes.len())?;
        let intended: BTreeSet<_> = r
            .entries()
            .iter()
            .map(|e| std::path::PathBuf::from(e.path.as_str()))
            .collect();
        let touched: BTreeSet<_> = batch
            .manifest_touched_paths
            .iter()
            .chain(batch.source_index_changes.keys())
            .cloned()
            .collect();
        if !touched.is_subset(&intended) || batch.manifest_audit_completed {
            return Err(ManifestEnrollmentError::Input);
        }
        for entry in r.entries() {
            ledger::validate_entry(&batch.tx, entry)?;
        }
        let result = batch.prepare_bounded_manifest_changes(r.current.revision())?;
        ledger::insert(&batch.tx, r, &result)?;
        let readback = ledger::read(&batch.tx, r)?.ok_or(ManifestEnrollmentError::Evidence)?;
        if readback != result {
            return Err(ManifestEnrollmentError::Evidence);
        }
        let mut committed_scope = Scope::from_fence(&self.current_scan_fence()?);
        if committed_scope.revision != result.revision {
            return Err(ManifestEnrollmentError::Evidence);
        }
        committed_scope.revision = r.current.revision();
        if committed_scope != Scope::from_fence(&r.current) {
            return Err(ManifestEnrollmentError::Input);
        }
        canceled(cancel)?;
        batch.validate_profile_owned_mutation()?;
        batch
            .tx
            .commit()
            .map_err(|_| ManifestEnrollmentError::CommitUncertain)?;
        self.database.maybe_checkpoint_owned_wal();
        let current = self
            .current_scan_fence()
            .map_err(|_| ManifestEnrollmentError::CommitUncertain)?;
        let mut scope = Scope::from_fence(&current);
        scope.revision = r.current.revision();
        if scope != Scope::from_fence(&r.current) || current.revision() != result.revision {
            return Err(ManifestEnrollmentError::CommitUncertain);
        }
        Ok(ManifestEnrollmentResult {
            receipt: self
                .issue_manifest_receipt(r, result)
                .map_err(|_| ManifestEnrollmentError::CommitUncertain)?,
            replayed: false,
        })
    }
}
