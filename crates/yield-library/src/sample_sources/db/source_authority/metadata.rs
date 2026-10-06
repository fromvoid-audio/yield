//! Explicit retained Current request metadata, checked before queue admission.
//!
//! Shared objects are charged conservatively per request. This counts immutable retained
//! allocations and fixed headers, not the profile's mutable registry, SQLite, OS descriptors,
//! allocator bookkeeping, or native effect capsules added by later bootstrap stages.

use super::*;

fn identity_bytes(identity: &SourceLeaseIdentity) -> Option<usize> {
    (identity.owner_identity.capacity() <= 256).then_some(identity.owner_identity.capacity())
}

impl SourceAuthorityRequest {
    /// Conservative owned Rust graph backing for another original participant.
    /// A journal Current keeper does not automatically follow this raw request Arc.
    /// This comparison data cannot validate registration or grant any effect.
    pub(crate) fn retained_graph_metadata_bytes(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<(usize, usize, Self)>()
            .checked_add(self.media_root.current_metadata_bytes()?)?
            .checked_add(self.database_root.current_metadata_bytes()?)?
            .checked_add(self.database_file_binding.current_metadata_bytes()?)?;
        bytes = bytes.checked_sub(2 * std::mem::size_of::<BoundDatabaseRoot>())?;
        if self.database_file_identity.capacity() > 256 {
            return None;
        }
        bytes = bytes
            .checked_add(self.database_file_identity.capacity())?
            .checked_add(identity_bytes(&self.lease_identity)?)?
            .checked_add(std::mem::size_of::<(usize, usize, AuthorityState)>())?
            .checked_add(identity_bytes(&self.state.lease_identity)?)?
            .checked_add(std::mem::size_of::<(usize, usize, AtomicBool)>())?
            .checked_add(std::mem::size_of::<(usize, usize, SourceEffectGate)>())?;
        let pending = self.state.pending_confirmation.lock().ok()?;
        if let Some(pending) = pending.as_ref() {
            bytes = bytes.checked_add(identity_bytes(&pending.lease_identity)?)?;
        }
        Some(bytes)
    }
}

impl SourceAuthorityRegistrationRequest {
    pub(crate) fn current_metadata_bytes(&self) -> Option<usize> {
        let request = &self.request;
        let mut bytes = std::mem::size_of::<Self>()
            .checked_add(request.retained_graph_metadata_bytes()?)?
            .checked_add(identity_bytes(&self.confirmation.lease_identity)?)?;
        // Identity copies in the live receipt/proof, returned receipt, and retired tombstone.
        // Each String clone is constructed from its original bounded contents, not its capacity.
        bytes = bytes.checked_add(5 * request.lease_identity.owner_identity.len())?;
        for identity in [
            request.media_root.identity(),
            request.database_root.identity(),
            request.database_file_identity(),
        ] {
            bytes = bytes.checked_add(identity.len())?;
        }
        // Both possible original source ancestors are cloned into actor volume custody before
        // acquisition effects. Their inline headers are already in SourceVolumeBinding; reserve
        // their explicit retained backing here before consuming the confirmation.
        for root in [&request.media_root, &request.database_root] {
            bytes = bytes.checked_add(
                root.current_metadata_bytes()?
                    .checked_sub(std::mem::size_of::<BoundDatabaseRoot>())?,
            )?;
        }
        Some(bytes)
    }
}
