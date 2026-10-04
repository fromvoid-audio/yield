//! Closed original-owner inputs. Observations can request preparation, never settlement.
use crate::sample_sources::db::write::SourceLeaseMaintenanceAllocation;
use crate::sample_sources::db::{SourceLayoutMetadataBudget, SourceTagAdmissionObservation};
use crate::sample_sources::{SourceRevision, wal_transport::SourceWalTransport};

/// Closed original-source preparation input. Every value comes from its actual source factory;
/// choosing a family cannot mint a positive preparation or database effect.
pub enum SourceInput {
    /// Genuine original audited source revision.
    Audited(SourceRevision),
    /// Genuine complete original tag scope and finite policy.
    Tag(SourceTagAdmissionObservation),
    /// Genuine original source layout allocation observation.
    Layout(SourceLayoutMetadataBudget),
    /// Genuine original pre-audit-capable lease allocation observation.
    Lease(SourceLeaseMaintenanceAllocation),
}
impl SourceInput {
    pub(super) fn revision(&self) -> Option<&SourceRevision> {
        match self {
            Self::Audited(revision) => Some(revision),
            Self::Tag(observation) => Some(observation.selection().revision()),
            Self::Layout(budget) => Some(budget.revision()),
            Self::Lease(_) => None,
        }
    }
    pub(super) fn database_identity(&self) -> &str {
        match self {
            Self::Lease(input) => input.subject().database_identity(),
            _ => self
                .revision()
                .expect("audited original input")
                .database_identity(),
        }
    }
    pub(super) fn deadline(&self) -> Option<std::time::Instant> {
        match self {
            Self::Audited(_) => None,
            Self::Tag(observation) => Some(observation.original_deadline()),
            Self::Layout(budget) => Some(budget.observation_deadline()),
            Self::Lease(input) => Some(input.deadline()),
        }
    }
    pub(super) fn retained_bytes(&self) -> Option<usize> {
        match self {
            Self::Audited(revision) => revision.retained_heap_bytes(),
            Self::Tag(observation) => observation.retained_input_bytes(),
            Self::Layout(budget) => budget.retained_input_bytes(),
            Self::Lease(input) => input.retained_input_bytes(),
        }
    }
}
/// Original requester and nonauthoritative participant/claim correlations for one source.
pub struct SourcePreparation {
    pub(super) requester: SourceWalTransport,
    pub(super) participant: super::super::ParticipantId,
    pub(super) database_claim: super::super::AdmissionClaimId,
    pub(super) sidecar_claim: super::super::AdmissionClaimId,
    pub(super) input: SourceInput,
    pub(super) database_device: Option<u64>,
    pub(super) profile_acquisition: Option<uuid::Uuid>,
}
impl SourcePreparation {
    /// Bind original source input to observed correlations. The complete command/plan factory
    /// validates coverage; only its actual producer can acquire and publish preparation.
    pub fn new(
        requester: SourceWalTransport,
        participant: super::super::ParticipantId,
        database_claim: super::super::AdmissionClaimId,
        sidecar_claim: super::super::AdmissionClaimId,
        input: SourceInput,
    ) -> Self {
        Self {
            requester,
            participant,
            database_claim,
            sidecar_claim,
            input,
            database_device: None,
            profile_acquisition: None,
        }
    }
}
