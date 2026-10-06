//! Selected namespace backing on the original reply, separate from observer credits.
//! Wiring requires the original detached StartupCredit implementation (OPT-2400).
use super::*;

pub(super) struct NamespaceCredits {
    // The enclosing reply wrapper frees its allocation before these final credits.
    _working: StartupCredit,
    _native: Option<StartupCredit>,
}

impl InputPermit {
    pub(in crate::operation_journal) fn can_retain_namespace_failure(&self) -> bool {
        let Some(backing) = self.historical_reply.as_ref() else {
            return false;
        };
        match backing.namespace.lock() {
            Ok(credit) => credit.is_none(),
            Err(poison) => {
                drop(poison.into_inner());
                self.shared.resource_admission.invalidate();
                self.shared
                    .lifecycle
                    .store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
                false
            }
        }
    }

    /// Consume the selected owned failure while its original loan remains alive.
    /// A missing consumer or occupied slot never overwrites another failure's credit.
    pub(in crate::operation_journal) fn retain_namespace_failure(
        &self,
        error: OperationJournalStoreError,
        working: InputPermit,
        native: Option<InputPermit>,
    ) -> OperationJournalStoreError {
        let Some(backing) = self.historical_reply.as_ref() else {
            drop(error);
            drop(native);
            drop(working);
            return OperationJournalStoreError::OrdinaryRebuildResourceBound;
        };
        let mut retained = match backing.namespace.lock() {
            Ok(retained) => retained,
            Err(poison) => {
                drop(poison.into_inner());
                self.shared.resource_admission.invalidate();
                self.shared
                    .lifecycle
                    .store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
                drop(error);
                drop(native);
                drop(working);
                return OperationJournalStoreError::OrdinaryRebuildResourceBound;
            }
        };
        if retained.is_some() {
            drop(retained);
            drop(error);
            drop(native);
            drop(working);
            return OperationJournalStoreError::OrdinaryRebuildResourceBound;
        }
        *retained = Some(NamespaceCredits {
            _working: working.into_startup_credit(),
            _native: native.map(InputPermit::into_startup_credit),
        });
        error
    }
}
