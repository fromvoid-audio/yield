//! Resolve original durable charge observations; selectors and bindings grant no native authority.
use super::super::admission_claims::{AdmissionClaim, AdmissionClaimId};
use super::super::capacity_provider::PublicationChargeBinding;
use super::super::full_capacity::FullCapacityAllocationClass;
use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Selector {
    Ordinary(AdmissionClaimId),
    Chunk(ChunkId),
}
fn refused(reason: impl Into<String>) -> OperationJournalOwnerError {
    OperationJournalOwnerError::StagingGrant {
        reason: reason.into(),
    }
}
pub(super) fn bind(
    store: &OperationJournalStore,
    provider: &CapacityProvider,
    selector: Selector,
    operation: OperationId,
    volume: &VolumeIdentity,
    bytes: u64,
) -> Result<PublicationChargeBinding, OperationJournalOwnerError> {
    let (ordinary, chunk) = match selector {
        Selector::Ordinary(id) => (Selector::Ordinary(id), None),
        Selector::Chunk(id) => {
            let chunk = store
                .read_chunk_claim(id)?
                .ok_or_else(|| refused("publication chunk charge is absent"))?;
            if chunk.operation_id() != operation || chunk.state() != ChunkClaimState::Issued {
                return Err(refused(
                    "publication chunk charge is not issued to this operation",
                ));
            }
            (Selector::Ordinary(chunk.claim_id()), Some(chunk))
        }
    };
    let Selector::Ordinary(claim_id) = ordinary else {
        return Err(refused("publication charge normalization failed"));
    };
    let retained = store
        .read_active_ordinary_claim(claim_id)
        .map_err(|error| refused(format!("publication charge readback failed: {error}")))?
        .ok_or_else(|| refused("publication ordinary charge is absent or released"))?;
    if chunk.as_ref().is_some_and(|chunk| {
        chunk.operation_id() != retained.operation_id()
            || chunk.volume() != retained.volume()
            || chunk.allocation_class() != retained.allocation_class()
            || chunk.allocated_bytes() != retained.allocated_bytes()
    }) {
        return Err(refused(
            "publication chunk differs from its exact original ordinary charge",
        ));
    }
    if retained.operation_id() != operation
        || retained.volume() != volume
        || retained.allocation_class() != FullCapacityAllocationClass::DestinationStaging
        || bytes == 0
        || retained.allocated_bytes() < bytes
    {
        return Err(refused(
            "publication charge differs from original operation, staging volume, class or bytes",
        ));
    }
    let claim = AdmissionClaim::new(
        retained.claim_id(),
        retained.volume().clone(),
        retained.allocation_class(),
        retained.allocated_bytes(),
    )
    .map_err(|_| refused("publication charge observation is invalid"))?;
    provider
        .bind_publication_charge(operation, &claim)
        .map_err(|error| {
            refused(format!(
                "original publication charge is unavailable: {error}"
            ))
        })
}
