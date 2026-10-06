//! Diagnostics borrow the actual retained store; no namespace or identity substitute.
use super::*;

impl OperationJournalStore {
    /// Same retained namespace with its original pool debit and selected-error keeper.
    /// Consume any returned owned error before dropping the diagnostic funding input.
    pub fn funded_retained_namespace_allocation_probe_for_test(
        &self,
        funding: &super::super::owner::JournalNamespaceFundingProbe,
        precommit: bool,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace_for_input(&funding.input, precommit)
    }

    /// Derive the original namespace loan, including selected returned-error backing.
    pub fn retained_namespace_working_bytes_for_test(&self) -> usize {
        namespace_backing::working_bytes(
            &self.database_root,
            &self.profile_guard,
            &self.database_path,
        )
        .and_then(|n| n.checked_add(std::mem::size_of::<namespace_backing::NamespaceLoan<'_>>()))
        .expect("original retained namespace bound")
    }

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
