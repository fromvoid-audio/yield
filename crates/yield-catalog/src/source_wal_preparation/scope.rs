//! Shared historical descriptor bytes retain their own portion of the existing context budget.
use super::*;
use yield_library::sample_sources::db::{
    SourceAuthorityRegistration,
    write::{SOURCE_WAL_BINDING_MAX_BYTES, SourceWalBinding},
};

// Subject clones have bounded identities (7.5 KiB maximum); the bounded encoding's Vec
// can have up to twice its 16 KiB length. Charge that staging before the capture begins.
const CAPTURE_BYTES: usize =
    3 * SOURCE_WAL_BINDING_MAX_BYTES + std::mem::size_of::<SourceWalBinding>() + 512;
const REPORT_BYTES: usize = std::mem::size_of::<Report>() + 64;

#[derive(Clone)]
pub(super) struct ScopeView(Arc<Report>);
struct Report {
    bytes: Box<[u8]>,
    _credit: ByteCredit,
}
impl ScopeView {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }
}
impl std::fmt::Debug for ScopeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeView")
            .field("encoded_bytes", &self.0.bytes.len())
            .finish()
    }
}
impl PartialEq for ScopeView {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}
impl Eq for ScopeView {}

struct ByteCredit {
    shared: Arc<Shared>,
    bytes: usize,
}
impl ByteCredit {
    fn resize(&mut self, bytes: usize) -> Result<(), CatalogSourceWalRejection> {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if q.closed {
            return Err(CatalogSourceWalRejection::Closed);
        }
        let remaining = q.bytes.checked_sub(self.bytes).expect("scope byte credit");
        let total = remaining
            .checked_add(bytes)
            .ok_or(CatalogSourceWalRejection::Full)?;
        if total > CATALOG_WAL_MAX_CONTEXT_BYTES {
            return Err(CatalogSourceWalRejection::Full);
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
impl Control {
    pub(super) fn capture_scope(
        &self,
        held: &SourceWalPreparation,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(SourceWalBinding, ScopeView), CatalogSourceWalRejection> {
        let mut credit = ByteCredit {
            shared: Arc::clone(&self.shared),
            bytes: 0,
        };
        credit.resize(CAPTURE_BYTES)?;
        // Accounting locks are gone before any original-owner validation or bounded SQL.
        let binding = held
            .capture_wal_binding(registration)
            .map_err(|e| owner::classify(&e))?;
        let bytes = binding
            .retained_memory_bytes()
            .and_then(|n| n.checked_add(binding.descriptor_bytes().len()))
            .and_then(|n| n.checked_add(REPORT_BYTES))
            .ok_or(CatalogSourceWalRejection::InvalidContext)?;
        // Charge the complete actual original binding and shared report before copying it.
        credit.resize(bytes)?;
        let report = ScopeView(Arc::new(Report {
            bytes: binding.descriptor_bytes().into(),
            _credit: credit,
        }));
        Ok((binding, report))
    }
}
