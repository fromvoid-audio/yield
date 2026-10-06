//! Original frozen lease RELEASE planning only, never renewal, Start or physical authority.
use crate::operation_journal::{OperationId, WorkUnitKey};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::sample_sources::db::write::source_writer) struct Input {
    operation: OperationId,
    work: WorkUnitKey,
    family_digest: [u8; 32],
    acquisition: Uuid,
}
/// Private historical original-producer comparison data. No deserializer or scalar factory.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SourceRetirementForwardPolicy {
    input: Input,
    preparation_nonce: Uuid,
    binding_digest: [u8; 32],
    ceilings: [u64; 3],
}
