//! Explicit retained Current request metadata, checked before queue admission.
//!
//! Shared objects are charged conservatively per request. This counts immutable retained
//! allocations and fixed headers, not the profile's mutable registry, SQLite, OS descriptors,
//! allocator bookkeeping, or native effect capsules added by later bootstrap stages.

use super::*;

fn identity_bytes(identity: &SourceLeaseIdentity) -> Option<usize> {
    (identity.owner_identity.capacity() <= 256).then_some(identity.owner_identity.capacity())
}

impl SourceAuthorityRegistrationRequest {
    pub(crate) fn current_metadata_bytes(&self) -> Option<usize> {
        let request = &self.request;
        let mut bytes = std::mem::size_of::<Self>()
            .checked_add(std::mem::size_of::<(usize, usize, SourceAuthorityRequest)>())?
            .checked_add(request.media_root.current_metadata_bytes()?)?
            .checked_add(request.database_root.current_metadata_bytes()?)?
            .checked_add(request.database_file_binding.current_metadata_bytes()?)?;
        // The two embedded BoundDatabaseRoots were already included in the request header.
        bytes = bytes.checked_sub(2 * std::mem::size_of::<BoundDatabaseRoot>())?;
        if request.database_file_identity.capacity() > 256 {
            return None;
        }
        bytes = bytes
            .checked_add(request.database_file_identity.capacity())?
            .checked_add(identity_bytes(&request.lease_identity)?)?
            .checked_add(identity_bytes(&self.confirmation.lease_identity)?)?
            .checked_add(std::mem::size_of::<(usize, usize, AuthorityState)>())?
            .checked_add(identity_bytes(&request.state.lease_identity)?)?
            .checked_add(std::mem::size_of::<(usize, usize, AtomicBool)>())?
            .checked_add(std::mem::size_of::<(usize, usize, SourceEffectGate)>())?;
        let pending = request.state.pending_confirmation.lock().ok()?;
        if let Some(pending) = pending.as_ref() {
            bytes = bytes.checked_add(identity_bytes(&pending.lease_identity)?)?;
        }
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
        Some(bytes)
    }
}
