//! A debit on the original pool without a strong OwnerShared back-reference.
//! The original pool's own allocation is permanently debited before Arc::new;
//! that debit ends only when the pool itself is destroyed, so no self-cycle.
use super::*;

pub(super) const REFUSAL: &str = "ordinary-claim startup resource envelope exceeded";
pub(super) const fn arc_bytes<T>() -> usize {
    let align = if std::mem::align_of::<T>() > std::mem::align_of::<usize>() {
        std::mem::align_of::<T>()
    } else {
        std::mem::align_of::<usize>()
    };
    let header = (2 * std::mem::size_of::<usize>() + align - 1) / align * align;
    header + (std::mem::size_of::<T>() + align - 1) / align * align
}
pub(super) const POOL_BYTES: usize = arc_bytes::<Mutex<InputBudget>>()
    + arc_bytes::<AtomicU8>()
    + arc_bytes::<ResourceAdmission>()
    + 2 * std::mem::size_of::<String>()
    + 4 * REFUSAL.len();

impl InputBudget {
    pub(super) fn shared_initial() -> Self {
        let mut budget = Self::new();
        budget.remaining[InputClass::Ordinary.index()] -= POOL_BYTES;
        budget
    }
}

pub(in crate::operation_journal) struct StartupCredit {
    lifecycle: Option<Arc<AtomicU8>>,
    resource: Option<Arc<ResourceAdmission>>,
    bytes: usize,
    publication: bool,
    // Last field: gates and their allocation headers disappear before the pool.
    pool: Arc<Mutex<InputBudget>>,
}
impl InputPermit {
    pub(in crate::operation_journal) fn into_startup_credit(mut self) -> StartupCredit {
        assert!(self.class == InputClass::Ordinary);
        assert!(
            self.historical_reply.is_none(),
            "terminal startup credits cannot retain replies"
        );
        let credit = StartupCredit {
            pool: self.shared.input_bytes.clone(),
            bytes: self.bytes,
            publication: self.publication,
            lifecycle: Some(self.shared.lifecycle.clone()),
            resource: Some(self.shared.resource_admission.clone()),
        };
        self.refund = false;
        credit
    }
}
impl std::fmt::Debug for StartupCredit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartupCredit")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
impl StartupCredit {
    #[cfg(test)]
    pub(super) fn bytes_for_test(&self) -> usize {
        self.bytes
    }

    fn fail_closed(&self) {
        if let Some(resource) = self.resource.as_ref() {
            resource.invalidate();
        }
        if let Some(lifecycle) = self.lifecycle.as_ref() {
            lifecycle.store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
        }
    }
}
impl Drop for StartupCredit {
    fn drop(&mut self) {
        let mut budget = match self.pool.lock() {
            Ok(budget) => budget,
            Err(error) => {
                drop(error);
                self.fail_closed();
                return;
            }
        };
        let after = budget.remaining[0].checked_add(self.bytes);
        let publication_after = if self.publication {
            budget
                .publication_remaining
                .checked_add(self.bytes)
                .filter(|bytes| *bytes <= PUBLICATION_INPUT_BYTES)
        } else {
            Some(budget.publication_remaining)
        };
        let mut restored = *budget;
        if let (Some(ordinary), Some(publication)) = (after, publication_after) {
            restored.remaining[0] = ordinary;
            restored.publication_remaining = publication;
        }
        if !budget.valid()
            || self.bytes == 0
            || after.is_none_or(|n| n > ORDINARY_INPUT_BYTES - POOL_BYTES)
            || publication_after.is_none()
            || !restored.valid()
        {
            budget.unavailable = true;
            drop(budget);
            self.fail_closed();
            return;
        }
        // These original gate allocations must disappear before their last pool holder.
        self.resource.take();
        self.lifecycle.take();
        *budget = restored;
    }
}

impl OwnerShared {
    pub(super) fn startup_input(
        self: &Arc<Self>,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        self.startup_input_with_publication(false)
    }
    pub(super) fn startup_input_with_publication(
        self: &Arc<Self>,
        publication: bool,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        let mut input = self.reserve_input_kind(
            InputClass::Ordinary,
            std::mem::size_of::<InputPermit>(),
            publication,
        )?;
        let header =
            self.reserve_input_kind(InputClass::Ordinary, HISTORICAL_REPLY_BYTES, publication)?;
        input.historical_reply = Some(HistoricalReply::new(HistoricalReplyBacking {
            consumer: Mutex::new(None),
            observer: Mutex::new(None),
            namespace: Mutex::new(None),
            control: Mutex::new(None),
            #[cfg(test)]
            consumer_refused: std::sync::atomic::AtomicBool::new(false),
            _header: header,
        }));
        input.fund_historical_consumer().map_err(|_| {
            OperationJournalOwnerQueueError::InputBytesFull {
                limit: ORDINARY_INPUT_BYTES,
                requested: super::super::store::HISTORICAL_DIAGNOSTIC_BYTES,
            }
        })?;
        Ok(input)
    }
    pub(super) fn mark_startup_unavailable(
        &self,
        reason: &impl std::fmt::Display,
        input: &InputPermit,
    ) {
        let bytes = display_bytes(reason);
        let credit = bytes.and_then(|n| input.reserve_historical_scratch(n).ok());
        let Some(credit) = credit else {
            self.mark_unavailable(REFUSAL);
            return;
        };
        self.resource_admission.invalidate();
        let mut output = self.unavailable.lock().unwrap_or_else(|p| p.into_inner());
        let mut retained = self
            .unavailable_startup_credit
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *output = Some(format!("{reason}"));
        *retained = Some(credit.into_startup_credit());
        self.lifecycle
            .store(OwnerLifecycle::Unavailable as u8, Ordering::Release);
    }
}

pub(super) fn display_bytes(value: &impl std::fmt::Display) -> Option<usize> {
    struct Count(usize);
    impl std::fmt::Write for Count {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0 = self.0.checked_add(text.len()).ok_or(std::fmt::Error)?;
            Ok(())
        }
    }
    let mut count = Count(0);
    std::fmt::write(&mut count, format_args!("{value}")).ok()?;
    count
        .0
        .checked_mul(2)?
        .checked_add(std::mem::size_of::<String>())
}

#[cfg(test)]
mod tests;
