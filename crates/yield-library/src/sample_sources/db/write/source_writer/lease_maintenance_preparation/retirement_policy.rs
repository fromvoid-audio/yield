//! Original frozen lease RELEASE planning only, never renewal, Start or physical authority.
use crate::lifecycle_allocation::LifecycleAllocationError;
use crate::operation_journal::{
    OperationId, WorkUnitKey, source_registry_retirement::SourceRegistryRetirementIntentArgs,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::sample_sources::db::write::source_writer) struct Input {
    operation: OperationId,
    work: WorkUnitKey,
    family_digest: [u8; 32],
    acquisition: Uuid,
}
impl Input {
    pub(in crate::sample_sources::db::write::source_writer) fn new(
        args: &SourceRegistryRetirementIntentArgs,
    ) -> Result<Self, LifecycleAllocationError> {
        args.validate()
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        Ok(Self {
            operation: args
                .operation_id()
                .map_err(|_| LifecycleAllocationError::Unknown)?,
            work: args.source_work(),
            family_digest: args
                .initial_forward_policy_digest()
                .map_err(|_| LifecycleAllocationError::Unknown)?,
            acquisition: args.lease_fence().subject().profile_acquisition(),
        })
    }
}

/// Private historical original-producer comparison data. No deserializer or scalar factory.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SourceRetirementForwardPolicy {
    input: Input,
    preparation_nonce: Uuid,
    binding_digest: [u8; 32],
    ceilings: [u64; 3],
}
