//! Historical original binding and complete shared observation bytes use the same context pool.
use super::*;
use crate::sample_sources::db::write::{SOURCE_WAL_BINDING_MAX_BYTES, SourceWalBinding};
// Peak phases are sequential, rather than three simultaneous descriptor encodings:
// audit decode holds <=24 KiB SQL text plus <=16 KiB decoded Vec backing; registered
// capture holds two <=7.25 KiB scalar subjects plus one <=16 KiB encoding; the final
// binding holds one subject and one encoding. The extra 8 KiB covers bounded lease
// rows (seven <=1 KiB fields), native identities and SQL/Arc headers. Original revision
// copies are separately charged threefold by the request. Current schema/runtime and
// tag membership comparisons borrow SQLite values. Descriptor/field limits stay intact.
pub(crate) const CAPTURE_BYTES: usize =
    2 * SOURCE_WAL_BINDING_MAX_BYTES + 8 * 1024 + std::mem::size_of::<SourceWalBinding>() + 512;
// Parked checks hold one bounded CONTROL lease row (seven <=1 KiB strings),
// fixed kernel map prefixes, current native identities and read-only SQL headers.
// This debit lives only during actual original validation, never as a readiness grant.
pub(super) const PARKED_BYTES: usize = 12 * 1024;
const REPORT_BYTES: usize = std::mem::size_of::<Report>() + 64;

#[derive(Clone)]
pub(super) struct ScopeView(Arc<Report>);
struct Report {
    // Original private binding drops before its memory credit; no duplicate descriptor buffer.
    binding: SourceWalBinding,
    _credit: ByteCredit,
}
impl ScopeView {
    pub(super) fn bytes(&self) -> &[u8] {
        self.0.binding.descriptor_bytes()
    }
    pub(super) fn binding(&self) -> &SourceWalBinding {
        &self.0.binding
    }
}
impl std::fmt::Debug for ScopeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeView")
            .field("encoded_bytes", &self.bytes().len())
            .finish()
    }
}
impl PartialEq for ScopeView {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}
impl Eq for ScopeView {}

pub(super) struct ByteCredit {
    shared: Arc<Shared>,
    bytes: usize,
    handshake: Option<crate::operation_journal::wal_admission::HandshakeCredit>,
}
impl std::fmt::Debug for ByteCredit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteCredit")
            .field("bytes", &self.bytes)
            .finish()
    }
}
impl ByteCredit {
    pub(super) fn new(shared: &Arc<Shared>) -> Self {
        Self {
            shared: Arc::clone(shared),
            bytes: 0,
            handshake: None,
        }
    }
    pub(super) fn in_handshake(
        shared: &Arc<Shared>,
        budget: Option<&Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
    ) -> Self {
        let mut credit = Self::new(shared);
        credit.handshake = budget.map(|budget| budget.credit());
        credit
    }
    pub(super) fn resize(&mut self, bytes: usize) -> Result<(), SourceWalTransportRejection> {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if q.closed {
            return Err(SourceWalTransportRejection::Closed);
        }
        let remaining = q.bytes.checked_sub(self.bytes).expect("scope byte credit");
        let total = remaining
            .checked_add(bytes)
            .ok_or(SourceWalTransportRejection::Full)?;
        if total > SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES {
            return Err(SourceWalTransportRejection::Full);
        }
        if let Some(credit) = &mut self.handshake {
            credit
                .resize(bytes)
                .map_err(|_| SourceWalTransportRejection::Full)?;
        }
        q.bytes = total;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for ByteCredit {
    fn drop(&mut self) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.bytes = q
            .bytes
            .checked_sub(self.bytes)
            .expect("scope byte credit release");
    }
}
pub(super) fn capture_scope(
    shared: &Arc<Shared>,
    held: &SourceWalPreparation,
    registration: &SourceAuthorityRegistration,
    handshake: Option<&Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
) -> Result<ScopeView, SourceWalTransportRejection> {
    let mut credit = ByteCredit::in_handshake(shared, handshake);
    credit.resize(CAPTURE_BYTES)?;
    // Accounting locks are gone before actual owner/native validation and bounded SQL.
    let binding = held
        .capture_wal_binding(registration)
        .map_err(|e| producer::classify(&e))?;
    let bytes = binding
        .retained_memory_bytes()
        .and_then(|n| n.checked_add(REPORT_BYTES))
        .ok_or(SourceWalTransportRejection::InvalidContext)?;
    credit.resize(bytes)?;
    Ok(ScopeView(Arc::new(Report {
        binding,
        _credit: credit,
    })))
}
