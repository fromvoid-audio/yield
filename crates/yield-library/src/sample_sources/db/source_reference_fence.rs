//! Source-owner-issued point-in-time reference read observations, never mutation authority.
use super::{SourceDbError, SourcePublicationFence, SourcePublicationFenceError, SourceRevision};
use std::sync::{Arc, Weak};
use thiserror::Error;

/// Fail-closed reference read observation errors.
#[derive(Debug, Error)]
pub enum SourceReferenceFenceError {
    /// The issuing writer instance has ended, even if its database/lease was reused.
    #[error("source reference writer lifecycle ended")]
    LifecycleEnded,
    /// Current audited revision differs from the issued observation.
    #[error("source reference revision changed")]
    RevisionChanged,
    /// The bounded audit metadata is absent, malformed, ambiguous or mismatched.
    #[error("source reference audit metadata is invalid")]
    MalformedAudit,
    /// An issuer observation violates the bounded reference contract.
    #[error("invalid source reference identity")]
    InvalidIdentity,
    /// Retained source ownership failed its read-only validation.
    #[error(transparent)]
    Publication(#[from] SourcePublicationFenceError),
    /// The retained database could not serve the observation.
    #[error(transparent)]
    Database(#[from] SourceDbError),
}

/// Exact audited revision scoped to one retained source writer and lifecycle.
///
/// No public constructor or deserialization exists. Validation is read-only, does not renew
/// ownership, and proves a point-in-time observation only. It neither holds a source mutation
/// gate nor authorizes file effects, journal completion or cross-database atomicity.
#[derive(Clone, Debug)]
pub struct SourceReferenceFence {
    revision: SourceRevision,
    publication: SourcePublicationFence,
    writer: Weak<()>,
}
impl SourceReferenceFence {
    pub(super) fn issue(
        revision: SourceRevision,
        publication: SourcePublicationFence,
        writer: &Arc<()>,
    ) -> Self {
        Self {
            revision,
            publication,
            writer: Arc::downgrade(writer),
        }
    }
    /// Borrow historical issuer/source/root/database/lifecycle/revision observations.
    pub fn source_revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Recheck issuer lifetime, retained source ownership and exact audited revision.
    /// The owner may change immediately after this call; validate at every consuming boundary.
    pub fn validate(&self) -> Result<(), SourceReferenceFenceError> {
        // Do not upgrade and retain the token: doing so would keep an ended writer apparently
        // live during the read. Only the SourceWriter owns a strong token.
        if self.writer.strong_count() == 0 {
            return Err(SourceReferenceFenceError::LifecycleEnded);
        }
        let (root, revision) = self.publication.read_reference_audit()?;
        if root != *self.revision.root_identity() {
            return Err(SourceReferenceFenceError::MalformedAudit);
        }
        if revision != self.revision.revision() {
            return Err(SourceReferenceFenceError::RevisionChanged);
        }
        if self.writer.strong_count() == 0 {
            return Err(SourceReferenceFenceError::LifecycleEnded);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
