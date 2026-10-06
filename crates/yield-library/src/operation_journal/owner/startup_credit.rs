//! A debit on the original pool without a strong OwnerShared back-reference.
//! The original pool's own allocation is permanently debited before Arc::new;
//! that debit ends only when the pool itself is destroyed, so no self-cycle.
use super::*;

pub(super) const REFUSAL: &str = "ordinary-claim startup resource envelope exceeded";
pub(super) const POOL_BYTES: usize = std::mem::size_of::<Mutex<InputBudget>>()
    + 2 * std::mem::size_of::<usize>()
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
    pool: Arc<Mutex<InputBudget>>,
    bytes: usize,
}
impl InputPermit {
    pub(in crate::operation_journal) fn into_startup_credit(mut self) -> StartupCredit {
        assert!(self.class == InputClass::Ordinary && !self.publication);
        assert!(
            self.historical_reply.is_none(),
            "terminal startup credits cannot retain replies"
        );
        let credit = StartupCredit {
            pool: self.shared.input_bytes.clone(),
            bytes: self.bytes,
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
impl Drop for StartupCredit {
    fn drop(&mut self) {
        let Ok(mut budget) = self.pool.lock() else {
            return;
        };
        let after = budget.remaining[0].checked_add(self.bytes);
        if !budget.valid()
            || self.bytes == 0
            || after.is_none_or(|n| n > ORDINARY_INPUT_BYTES - POOL_BYTES)
        {
            budget.unavailable = true;
            return;
        }
        budget.remaining[0] = after.unwrap();
    }
}

impl OwnerShared {
    pub(super) fn startup_input(
        self: &Arc<Self>,
    ) -> Result<InputPermit, OperationJournalOwnerQueueError> {
        let mut input =
            self.reserve_input(InputClass::Ordinary, std::mem::size_of::<InputPermit>())?;
        let header = self.reserve_input(InputClass::Ordinary, HISTORICAL_REPLY_BYTES)?;
        input.historical_reply = Some(HistoricalReply::new(HistoricalReplyBacking {
            consumer: Mutex::new(None),
            observer: Mutex::new(None),
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
