//! Combined local accounting and original post-reservation input custody.
use super::WalPreparationError;
use super::collector::journal_input::JournalInput;
use std::sync::{Arc, Mutex};

pub(crate) const MAX_BYTES: usize = 64 * 1024;

/// Every real alias retains the original journal debit. No raw Arc/Weak escapes.
pub(crate) struct HandshakeBudget(Option<Arc<Backing>>);
pub(super) const BACKING_BYTES: usize = std::mem::size_of::<Backing>() + 64;
struct Backing {
    bytes: Mutex<usize>,
    // The raw budget allocation is freed by into_inner before this custody drops.
    journal_input: Mutex<Option<JournalInput>>,
}
impl Clone for HandshakeBudget {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Drop for HandshakeBudget {
    fn drop(&mut self) {
        if let Some(backing) = self.0.take().and_then(Arc::into_inner) {
            drop(backing);
        }
    }
}
impl std::fmt::Debug for HandshakeBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandshakeBudget")
            .field("bytes", &self.backing().bytes)
            .finish_non_exhaustive()
    }
}
impl HandshakeBudget {
    fn backing(&self) -> &Backing {
        self.0.as_deref().expect("live original handshake budget")
    }
    pub(super) fn new() -> Self {
        Self(Some(Arc::new(Backing {
            bytes: Mutex::new(0),
            journal_input: Mutex::new(None),
        })))
    }
    pub(super) fn bind_journal_input(
        &self,
        input: JournalInput,
    ) -> Result<(), WalPreparationError> {
        let mut retained = self
            .backing()
            .journal_input
            .lock()
            .map_err(|_| WalPreparationError::Full)?;
        if retained.is_some() {
            return Err(WalPreparationError::InvalidInput);
        }
        *retained = Some(input);
        Ok(())
    }
    pub(crate) fn credit(&self) -> HandshakeCredit {
        HandshakeCredit {
            budget: self.clone(),
            bytes: 0,
        }
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        *self.backing().bytes.lock().unwrap()
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn test_accounting_guard(&self) -> std::sync::MutexGuard<'_, usize> {
        self.backing().bytes.lock().unwrap()
    }
}
#[derive(Debug)]
pub(crate) struct HandshakeCredit {
    budget: HandshakeBudget,
    bytes: usize,
}
impl HandshakeCredit {
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), ()> {
        let mut current = self.budget.backing().bytes.lock().map_err(|_| ())?;
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
        if let Ok(mut current) = self.budget.backing().bytes.lock() {
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
