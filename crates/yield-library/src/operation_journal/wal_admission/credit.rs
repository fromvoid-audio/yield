//! One combined handshake ceiling, additionally to each original owner's local ceiling.
use std::sync::{Arc, Mutex};

pub(crate) const MAX_BYTES: usize = 64 * 1024;
#[derive(Debug)]
pub(crate) struct HandshakeBudget {
    bytes: Mutex<usize>,
}
impl HandshakeBudget {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(0),
        })
    }
    pub(crate) fn credit(self: &Arc<Self>) -> HandshakeCredit {
        HandshakeCredit {
            budget: Arc::clone(self),
            bytes: 0,
        }
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        *self.bytes.lock().unwrap()
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn test_accounting_guard(&self) -> std::sync::MutexGuard<'_, usize> {
        self.bytes.lock().unwrap()
    }
}
/// Private non-Clone debit. Immutable actor replies share the same original credit via Arc.
#[derive(Debug)]
pub(crate) struct HandshakeCredit {
    budget: Arc<HandshakeBudget>,
    bytes: usize,
}
impl HandshakeCredit {
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), ()> {
        let mut current = self.budget.bytes.lock().map_err(|_| ())?;
        let next = current
            .checked_sub(self.bytes)
            .ok_or(())?
            .checked_add(bytes)
            .filter(|n| *n <= MAX_BYTES)
            .ok_or(())?;
        *current = next;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for HandshakeCredit {
    fn drop(&mut self) {
        // Poison cannot refund another holder's debit or reopen capacity. The handshake
        // stays unavailable; no effect/claim/rollback outcome follows from this drop.
        if let Ok(mut current) = self.budget.bytes.lock() {
            *current = current
                .checked_sub(self.bytes)
                .expect("original handshake debit");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_owner_memory_credit_checks_combined_overflow_and_last_shared_holder() {
        let budget = HandshakeBudget::new();
        let mut native = budget.credit();
        native.resize(MAX_BYTES - 1).unwrap();
        let mut reply = budget.credit();
        reply.resize(2).unwrap_err();
        assert_eq!(budget.used(), MAX_BYTES - 1);
        reply.resize(1).unwrap();
        assert_eq!(budget.used(), MAX_BYTES);
        reply.resize(usize::MAX).unwrap_err();
        assert_eq!(budget.used(), MAX_BYTES);
        let copied = Arc::new(reply);
        let original = Arc::clone(&copied);
        drop(original);
        assert_eq!(budget.used(), MAX_BYTES);
        drop(native);
        assert_eq!(budget.used(), 1);
        drop(copied);
        assert_eq!(budget.used(), 0);
    }
}
