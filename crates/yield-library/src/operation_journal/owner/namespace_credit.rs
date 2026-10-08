//! Selected namespace/control/observer backing retained on the original reply.
//! Wiring requires the original detached StartupCredit implementation (OPT-2400).
use super::*;

/// Original command failure and its namespace backing after request consumption.
/// This owns no authority and cannot be cloned or expose its backing keeper.
pub struct RetainedCommandError {
    error: Option<Box<OperationJournalOwnerError>>,
    reply: Option<HistoricalReply>,
}

impl RetainedCommandError {
    /// Borrow the original typed command failure without detaching its backing.
    pub fn original_error(&self) -> &OperationJournalOwnerError {
        self.error
            .as_deref()
            .expect("live original namespace failure")
    }
}

impl std::fmt::Debug for RetainedCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.original_error(), f)
    }
}

impl std::fmt::Display for RetainedCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.original_error(), f)
    }
}

impl std::error::Error for RetainedCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.original_error())
    }
}

impl Drop for RetainedCommandError {
    fn drop(&mut self) {
        // Destroy the original owned error and its Box allocation before releasing
        // the original reply allocation or any selected namespace backing debit.
        drop(self.error.take());
        drop(self.reply.take());
    }
}

pub(super) fn retain_result_error<T>(
    result: Result<T, OperationJournalOwnerError>,
    reply: Option<HistoricalReply>,
) -> Result<T, OperationJournalOwnerError> {
    match (result, reply) {
        (Err(error), Some(reply)) => {
            let namespace_selected = reply
                .namespace
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_some();
            // Separate statements drop the namespace guard before the control lock.
            let control_selected = reply
                .control
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_some();
            let observer_selected = reply
                .observer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_some();
            if namespace_selected || control_selected || observer_selected {
                // The selected namespace, control or historical observer prepays this exact
                // Box size before its original observation. The live reply retains that debit here.
                Err(OperationJournalOwnerError::RetainedBacking(
                    RetainedCommandError {
                        error: Some(Box::new(error)),
                        reply: Some(reply),
                    },
                ))
            } else {
                Err(error)
            }
        }
        (result, _) => result,
    }
}

/// Original pool/input for normal-build namespace allocation diagnostics only.
/// Construction installs no capacity, admission, execution or settlement authority.
#[cfg(any(test, feature = "test-support"))]
pub struct JournalNamespaceFundingProbe {
    pub(in crate::operation_journal) input: InputPermit,
}

#[cfg(any(test, feature = "test-support"))]
impl JournalNamespaceFundingProbe {
    /// Construct the original diagnostic input/reply outside the measured interval.
    pub fn new() -> Self {
        let (commands, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        Self {
            input: shared
                .startup_input()
                .expect("original diagnostic startup input"),
        }
    }

    /// Exercise the exact publication debit class without issuing any live grant.
    #[cfg(test)]
    pub(in crate::operation_journal) fn new_publication() -> Self {
        let (commands, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        Self {
            input: shared
                .startup_input_with_publication(true)
                .expect("original publication diagnostic input"),
        }
    }
    /// Observe both original counters without retaining the original owner.
    #[cfg(test)]
    pub(in crate::operation_journal) fn publication_observer(
        &self,
    ) -> impl Fn() -> (usize, usize) + use<> {
        let pool = self.input.shared.input_bytes.clone();
        move || {
            let budget = pool.lock().unwrap();
            (budget.remaining[0], budget.publication_remaining)
        }
    }
    /// Observe original ordinary-pool availability; this grants no capacity authority.
    pub fn remaining_bytes(&self) -> usize {
        self.input.shared.input_bytes.lock().unwrap().remaining[0]
    }

    /// Borrow-free scalar observer of this exact original pool and its permanent
    /// baseline. Cloning the original Arc allocates no replacement pool or grant.
    pub fn pool_bytes_observer_for_test(&self) -> impl Fn() -> (usize, usize) + use<> {
        let pool = self.input.shared.input_bytes.clone();
        move || {
            (
                pool.lock().unwrap().remaining[InputClass::Ordinary.index()],
                InputBudget::shared_initial().remaining[InputClass::Ordinary.index()],
            )
        }
    }

    /// Retain a same-Store selected failure through diagnostic caller destruction.
    pub fn retain_command_error_for_test(
        &self,
        error: OperationJournalStoreError,
    ) -> OperationJournalOwnerError {
        retain_result_error::<()>(Err(error.into()), self.input.historical_reply.clone())
            .expect_err("original selected diagnostic failure")
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for JournalNamespaceFundingProbe {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Test-only original scalar slot, never a Source pin, native grant or effect receipt.
#[cfg(test)]
pub(in crate::operation_journal) struct NoEffectSlotFixture {
    pending: publication_fence::PendingFence,
    registry: publication_fence::Registry,
}
#[cfg(test)]
impl NoEffectSlotFixture {
    pub(in crate::operation_journal) fn issuer(&self) -> (Uuid, u64) {
        self.registry.issuance_observation(&self.pending).unwrap()
    }
    pub(in crate::operation_journal) fn complete(&self, fingerprint: [u8; 32], reason: &str) {
        let digest =
            crate::file_operation_owner::publication_no_effect_digest(1, fingerprint, reason);
        self.pending
            .complete_before_effect_detail(digest, true, 1, reason);
    }
    pub(in crate::operation_journal) fn settle(
        &self,
        persist: impl FnMut(&NoEffectCompletion) -> bool,
    ) {
        self.registry.settle_no_effects(persist);
    }
    pub(in crate::operation_journal) fn unresolved(&self) -> bool {
        self.registry.unresolved()
    }
    // Actual actor polling with original scalar custody and real SQLite; no Native grant.
    pub(in crate::operation_journal) fn poll_original_actor(
        &mut self,
        store: OperationJournalStore,
    ) -> OperationJournalStore {
        let mut runtime = super::OwnerRuntime::new(store).expect("original actor runtime");
        std::mem::swap(&mut runtime.publication_fences, &mut self.registry);
        runtime.poll_publication_completions();
        std::mem::swap(&mut runtime.publication_fences, &mut self.registry);
        runtime.store.take().expect("original actor store")
    }
}
#[cfg(test)]
impl JournalNamespaceFundingProbe {
    pub(in crate::operation_journal) fn original_no_effect_slot(
        &self,
        operation: OperationId,
        output: Uuid,
    ) -> NoEffectSlotFixture {
        let mut registry = publication_fence::Registry::new();
        let pending = registry.issue(operation, &self.input.shared).unwrap();
        pending.bind_reserved_output(output).unwrap();
        NoEffectSlotFixture { pending, registry }
    }
}
