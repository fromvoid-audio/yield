//! Private original input custody; raw Arc/Weak handles never escape.
use crate::operation_journal::owner::InputPermit;
use std::sync::Arc;
pub(in crate::operation_journal) struct JournalInput(Option<Arc<InputPermit>>);
impl JournalInput {
    pub(in crate::operation_journal) fn belongs_to(
        &self,
        client: &crate::operation_journal::owner::OperationJournalClient,
    ) -> bool {
        client.owns_wal_input(self.0.as_deref().expect("live original WAL debit"))
    }

    pub(in crate::operation_journal) fn new(input: InputPermit) -> Self {
        Self(Some(Arc::new(input)))
    }
}
impl Clone for JournalInput {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Drop for JournalInput {
    fn drop(&mut self) {
        if let Some(input) = self.0.take().and_then(Arc::into_inner) {
            drop(input);
        }
    }
}
