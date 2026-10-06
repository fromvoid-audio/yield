//! Diagnostics borrow the actual retained store; no namespace or identity substitute.
use super::*;

impl OperationJournalStore {
    /// Validate the original retained namespace for allocation diagnostics.
    /// This observation supplies no admission, effect or native qualification authority.
    pub fn retained_namespace_allocation_probe_for_test(
        &self,
        precommit: bool,
    ) -> Result<(), OperationJournalStoreError> {
        if precommit {
            self.validate_namespace_for_precommit()
        } else {
            self.validate_namespace()
        }
    }

    /// Copy the actual retained pathname outside the measured validation interval.
    pub fn retained_namespace_path_for_test(&self) -> PathBuf {
        self.database_path.clone()
    }

    /// Enter or leave an original read transaction for namespace-order diagnostics.
    pub fn retained_namespace_transaction_for_test(
        &self,
        active: bool,
    ) -> Result<(), OperationJournalStoreError> {
        self.connection
            .execute_batch(if active { "BEGIN" } else { "ROLLBACK" })?;
        Ok(())
    }
}
