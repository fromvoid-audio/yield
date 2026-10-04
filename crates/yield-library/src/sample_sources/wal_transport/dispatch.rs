//! Closed typed dispatch for the actual catalog source producer.
use super::*;
/// Only original commands issued by the shared source inbox.
pub enum SourceWalProducerCommand {
    /// Genuine audited, tag or layout source command.
    Audited(SourceWalTransportCommand),
    /// Genuine pre-audit-capable lease command.
    Lease(SourceLeaseWalTransportCommand),
}
impl SourceWalProducerCommand {
    /// Original source identity, never a fabricated audit revision.
    pub fn source_id(&self) -> &str {
        match self {
            Self::Audited(c) => c.expected_revision().source_id().as_str(),
            Self::Lease(c) => c.subject().source_id(),
        }
    }
    /// Original lifecycle generation.
    pub fn lifecycle_generation(&self) -> u64 {
        match self {
            Self::Audited(c) => c.expected_revision().lifecycle_generation(),
            Self::Lease(c) => c.subject().lifecycle_generation(),
        }
    }
    /// Consume the exact native source writer into typed original custody.
    pub fn acquire(self, writer: SourceWriter) -> SourceWalProducerOwner {
        match self {
            Self::Audited(c) => SourceWalProducerOwner::Audited(c.acquire(writer)),
            Self::Lease(c) => SourceWalProducerOwner::Lease(c.acquire(writer)),
        }
    }
    /// Original command deadline.
    pub fn deadline(&self) -> Instant {
        match self {
            Self::Audited(c) => c.deadline(),
            Self::Lease(c) => c.deadline(),
        }
    }
    /// Original command is cancelled.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Audited(c) => c.is_cancelled(),
            Self::Lease(c) => c.is_cancelled(),
        }
    }
    /// Reject only pending custody.
    pub fn reject(self, reason: SourceWalTransportRejection) {
        match self {
            Self::Audited(c) => c.reject(reason),
            Self::Lease(c) => c.reject(reason),
        }
    }
}
/// Closed original native owner families. Each variant remains unforgeable.
pub enum SourceWalProducerOwner {
    /// Original audited/tag/layout custody.
    Audited(SourceWalTransportOwner),
    /// Original lease preparation/failure custody.
    Lease(SourceLeaseWalTransportOwner),
}
impl SourceWalProducerOwner {
    /// Inspect original owner deadline.
    pub fn deadline(&self) -> Instant {
        match self {
            Self::Audited(o) => o.deadline(),
            Self::Lease(o) => o.deadline(),
        }
    }
    /// Inspect original owner is cancelled.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Audited(o) => o.is_cancelled(),
            Self::Lease(o) => o.is_cancelled(),
        }
    }
    /// Inspect original owner is prepared.
    pub fn is_prepared(&self) -> bool {
        match self {
            Self::Audited(o) => o.is_prepared(),
            Self::Lease(o) => o.is_prepared(),
        }
    }
    /// Inspect original owner rejection.
    pub fn rejection(&self) -> Option<SourceWalTransportRejection> {
        match self {
            Self::Audited(o) => o.rejection(),
            Self::Lease(o) => o.rejection(),
        }
    }
    /// Inspect original owner database identity.
    pub fn database_identity(&self) -> Option<&str> {
        match self {
            Self::Audited(o) => o.database_identity(),
            Self::Lease(o) => o.database_identity(),
        }
    }
    /// Inspect original owner outcome observation.
    pub fn outcome_observation(&self) -> SourceWalOwnerObservation {
        match self {
            Self::Audited(o) => o.outcome_observation(),
            Self::Lease(o) => o.outcome_observation(),
        }
    }
    /// Inspect original owner source authority close ready.
    pub fn source_authority_close_ready(&self) -> bool {
        match self {
            Self::Audited(o) => o.source_authority_close_ready(),
            Self::Lease(o) => o.source_authority_close_ready(),
        }
    }
    /// Validate the genuine original owner and registration.
    pub fn publish_prepared(
        &mut self,
        registration: Option<&SourceAuthorityRegistration>,
    ) -> Result<(), SourceWalTransportRejection> {
        match self {
            Self::Audited(o) => o.publish_prepared(registration),
            Self::Lease(o) => o.publish_prepared(registration),
        }
    }
    /// Validate the genuine original owner and registration.
    pub fn revalidate(
        &self,
        registration: Option<&SourceAuthorityRegistration>,
    ) -> Result<(), SourceWalTransportRejection> {
        match self {
            Self::Audited(o) => o.revalidate(registration),
            Self::Lease(o) => o.revalidate(registration),
        }
    }
    /// Begin genuine pre-intent cleanup.
    pub fn request_abort(&mut self, reason: Option<SourceWalTransportRejection>) {
        match self {
            Self::Audited(o) => o.request_abort(reason),
            Self::Lease(o) => o.request_abort(reason),
        }
    }
    /// Return only the confirmed original reusable writer; keep failed custody private.
    pub fn into_writer(self) -> Result<SourceWriter, Self> {
        match self {
            Self::Audited(o) => o.into_writer().map_err(Self::Audited),
            Self::Lease(o) => o.into_writer().map_err(Self::Lease),
        }
    }
    /// Retire only through the original draining source authority's real acknowledgement.
    pub fn close_for_retirement(
        self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        match self {
            Self::Audited(o) => o.close_for_retirement(authority),
            Self::Lease(o) => o.close_for_retirement(authority),
        }
    }
}
