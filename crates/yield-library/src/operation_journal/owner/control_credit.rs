//! Selected transaction-control credits, separate from namespace/observer loans.
use super::startup_credit::StartupCredit;
use super::*;

pub(super) struct ControlCredits {
    // The reply wrapper deallocates its original Arc before these terminal loans.
    _working: StartupCredit,
    _native: Option<StartupCredit>,
}

#[cfg(test)]
impl ControlCredits {
    pub(super) fn bytes_for_test(&self) -> usize {
        self._working.bytes_for_test()
            + self
                ._native
                .as_ref()
                .map_or(0, StartupCredit::bytes_for_test)
    }
}

impl InputPermit {
    #[cfg(any(test, feature = "test-support"))]
    pub(in crate::operation_journal) fn control_probe_remaining(&self) -> usize {
        self.shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()]
    }

    pub(in crate::operation_journal) fn can_retain_control_failure(&self) -> bool {
        let Some(backing) = self.historical_reply.as_ref() else {
            return false;
        };
        match backing.control.lock() {
            Ok(retained) => retained.is_none(),
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

    pub(in crate::operation_journal) fn retain_control_failure(
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
        let mut retained = match backing.control.lock() {
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
        *retained = Some(ControlCredits {
            _working: working.into_startup_credit(),
            _native: native.map(InputPermit::into_startup_credit),
        });
        error
    }
}
