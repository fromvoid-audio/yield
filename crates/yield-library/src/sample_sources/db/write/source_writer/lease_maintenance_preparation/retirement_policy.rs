//! Original frozen lease RELEASE planning only, never renewal, Start or physical authority.
use crate::lifecycle_allocation::LifecycleAllocationError;
use crate::operation_journal::{
    OperationId, WorkUnitKey,
    source_lease_binding::{MAX_SOURCE_LEASE_SCOPE_BYTES, StoredSourceLeaseFence},
    source_registry_retirement::SourceRegistryRetirementIntentArgs,
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
impl SourceRetirementForwardPolicy {
    // Only the original preparation's binding-capture module calls this after retaining
    // and revalidating genuine Registered custody. Input itself is never a policy grant.
    pub(super) fn capture(
        input: Input,
        binding: &StoredSourceLeaseFence,
        nonce: Uuid,
        encoded: &[u8],
    ) -> Result<Self, LifecycleAllocationError> {
        if nonce.is_nil()
            || binding.preparation_nonce() != nonce
            || input.acquisition != binding.subject().profile_acquisition()
            || !binding.lifecycle_observed_registered()
            || encoded.is_empty()
            || encoded.len() > MAX_SOURCE_LEASE_SCOPE_BYTES
            || !components_valid(binding.write_ceilings(), binding.allocation_unit())
            || binding_digest(encoded) != stored_digest(binding)?
        {
            return Err(LifecycleAllocationError::Unknown);
        }
        Ok(Self {
            input,
            preparation_nonce: nonce,
            binding_digest: binding_digest(encoded),
            ceilings: binding.write_ceilings(),
        })
    }

    /// Historical comparison only. The caller first validates the genuine Global policy
    /// against the complete frozen family, then compares its digest without reserializing
    /// that family or allocating another Source-side comparison buffer.
    pub(crate) fn matches_original(
        &self,
        args: &SourceRegistryRetirementIntentArgs,
        family_digest: &[u8; 32],
        binding: &StoredSourceLeaseFence,
        encoded: &[u8],
    ) -> bool {
        args.operation_id()
            .is_ok_and(|operation| operation == self.input.operation)
            && args.source_work() == self.input.work
            && self.input.family_digest == *family_digest
            && self.input.acquisition == args.lease_fence().subject().profile_acquisition()
            && self.input.acquisition == binding.subject().profile_acquisition()
            && binding.matches_initial_policy(args.lease_fence())
            && binding.preparation_nonce() == self.preparation_nonce
            && !encoded.is_empty()
            && encoded.len() <= MAX_SOURCE_LEASE_SCOPE_BYTES
            && binding_digest(encoded) == self.binding_digest
            && stored_digest(binding).is_ok_and(|digest| digest == self.binding_digest)
            && self.ceilings == binding.write_ceilings()
            && components_valid(self.ceilings, binding.allocation_unit())
    }

    pub(crate) fn component_ceilings(&self) -> [u64; 3] {
        self.ceilings
    }
}

fn binding_digest(encoded: &[u8]) -> [u8; 32] {
    blake3::derive_key("yield.source-retirement-release-binding.v1", encoded)
}
// Stream the known bounded original fence into the same digest without constructing
// another encoding Vec. A caller-supplied byte buffer cannot change the sealed scope.
fn stored_digest(binding: &StoredSourceLeaseFence) -> Result<[u8; 32], LifecycleAllocationError> {
    struct DigestWriter<'a>(&'a mut blake3::Hasher);
    impl std::io::Write for DigestWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = blake3::Hasher::new_derive_key("yield.source-retirement-release-binding.v1");
    serde_json::to_writer(DigestWriter(&mut hash), binding)
        .map_err(|_| LifecycleAllocationError::Unknown)?;
    Ok(*hash.finalize().as_bytes())
}
fn components_valid(per: [u64; 3], unit: u64) -> bool {
    unit > 0
        && per
            .iter()
            .all(|n| *n > 0 && *n <= i64::MAX as u64 && *n % unit == 0)
        && per[1]
            .checked_add(per[2])
            .is_some_and(|sum| sum <= i64::MAX as u64)
}

#[cfg(test)]
mod tests {
    use super::components_valid;

    #[test]
    fn source_release_component_pair_stays_in_sqlite_range() {
        let max = i64::MAX as u64;
        assert!(components_valid([1, max - 1, 1], 1));
        assert!(!components_valid([1, max, 1], 1));
        assert!(!components_valid([1, u64::MAX, 1], 1));
        assert!(!components_valid([max + 1, 1, 1], 1));
    }

    #[test]
    fn source_release_components_require_complete_aligned_caps() {
        assert!(components_valid([4096; 3], 4096));
        assert!(!components_valid([4096; 3], 0));
        assert!(!components_valid([4096, 4097, 4096], 4096));
        for missing in 0..3 {
            let mut per = [4096; 3];
            per[missing] = 0;
            assert!(!components_valid(per, 4096));
        }
    }
}
