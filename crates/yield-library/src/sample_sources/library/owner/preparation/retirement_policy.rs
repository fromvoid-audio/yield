//! Original initial-forward planning only; no Start, effect, compensation or physical grant.
use super::*;
use crate::operation_journal::{
    OperationId, WorkUnitKey, source_registry_retirement::SourceRegistryRetirementIntentArgs,
};
use serde::Serialize;
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Input {
    operation: OperationId,
    work: [WorkUnitKey; 2],
    family_digest: [u8; 32],
    acquisition: uuid::Uuid,
}
struct Digest(blake3::Hasher, usize);
impl Write for Digest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.1 = self.1.checked_add(bytes.len()).filter(|n| *n <= crate::operation_journal::source_registry_retirement::MAX_SOURCE_REGISTRY_RETIREMENT_BYTES).ok_or_else(|| io::Error::other("retirement policy bound"))?;
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Input {
    pub(super) fn new(
        args: &SourceRegistryRetirementIntentArgs,
    ) -> Result<Self, GlobalPreparationError> {
        args.validate()
            .map_err(|_| GlobalPreparationError::InputBound)?;
        let mut digest = Digest(
            blake3::Hasher::new_derive_key("yield.global-retirement-initial-forward.v1"),
            0,
        );
        serde_json::to_writer(&mut digest, args).map_err(|_| GlobalPreparationError::InputBound)?;
        Ok(Self {
            operation: args
                .operation_id()
                .map_err(|_| GlobalPreparationError::InputBound)?,
            work: [args.draining_work(), args.retired_work()],
            family_digest: *digest.0.finalize().as_bytes(),
            acquisition: args.lease_fence().subject().profile_acquisition(),
        })
    }
}

/// Typed historical policy captured on the original entered global worker. Each frozen
/// sequential transition has its own complete component budget. Copies cannot acknowledge
/// Start or issue physical capacity, compensation, a permit or any transition.
/// ```compile_fail
/// use yield_library::sample_sources::library::GlobalRetirementForwardPolicy;
/// let invented = GlobalRetirementForwardPolicy {};
/// ```
/// ```compile_fail
/// use yield_library::sample_sources::library::GlobalRetirementForwardPolicy;
/// fn decode<T: serde::de::DeserializeOwned>() {}
/// decode::<GlobalRetirementForwardPolicy>();
/// ```
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct GlobalRetirementForwardPolicy {
    version: u8,
    input: Input,
    registration: uuid::Uuid,
    registration_generation: u64,
    preparation_nonce: uuid::Uuid,
    per_transition: [u64; 3],
    total: [u64; 3],
}
impl GlobalRetirementForwardPolicy {
    /// Exact frozen operation correlation; not a newly allocated journal identity.
    pub fn operation_id(&self) -> OperationId {
        self.input.operation
    }
    /// Draining then Retired; these budgets cannot be reused for concurrent/compensation work.
    pub fn work_keys(&self) -> &[WorkUnitKey; 2] {
        &self.input.work
    }
    /// Versioned digest of the complete frozen retirement family.
    pub fn family_digest(&self) -> &[u8; 32] {
        &self.input.family_digest
    }
    /// MAIN, WAL and SHM ceiling for EACH sequential frozen transition, not available space.
    pub fn per_transition_ceilings(&self) -> [u64; 3] {
        self.per_transition
    }
    /// Checked sum of the two distinct sequential forward budgets.
    pub fn total_forward_ceilings(&self) -> [u64; 3] {
        self.total
    }
}
fn totals(per: [u64; 3], unit: u64) -> Result<[u64; 3], GlobalPreparationError> {
    let mut total = [0; 3];
    for (target, ceiling) in total.iter_mut().zip(per) {
        *target = ceiling
            .checked_mul(2)
            .filter(|n| unit > 0 && ceiling > 0 && ceiling % unit == 0 && *n <= i64::MAX as u64)
            .ok_or(GlobalPreparationError::InputBound)?;
    }
    total[1]
        .checked_add(total[2])
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or(GlobalPreparationError::InputBound)?;
    Ok(total)
}
pub(super) fn capture(
    request: &GlobalPreparationRequest,
    nonce: uuid::Uuid,
    allocation: Option<&crate::lifecycle_allocation::LifecycleDatabaseAllocation>,
) -> Result<Option<GlobalRetirementForwardPolicy>, GlobalPreparationError> {
    let Some(input) = request.retirement else {
        return Ok(None);
    };
    let allocation = allocation.ok_or(GlobalPreparationError::InputBound)?;
    if !request.lifecycle_policy
        || !request.wal_binding
        || allocation.role()
            != crate::lifecycle_allocation::LifecycleAllocationRole::GlobalReferences
        || uuid::Uuid::parse_str(request.owner.acquisition.as_str()).ok() != Some(input.acquisition)
        || nonce.is_nil()
    {
        // Malformed family/policy requests do not mean original native custody changed.
        // The worker confirms empty rollback and restores settings before reuse.
        return Err(GlobalPreparationError::InputBound);
    }
    let objects = [allocation.main(), allocation.wal(), allocation.shm()];
    let unit = objects[0].allocation_unit();
    if objects
        .iter()
        .any(|o| o.device() != objects[0].device() || o.allocation_unit() != unit)
    {
        return Err(GlobalPreparationError::DatabaseBinding);
    }
    let per_transition = objects.map(|o| o.write_ceiling());
    let total = totals(per_transition, unit)?;
    Ok(Some(GlobalRetirementForwardPolicy {
        version: 1,
        input,
        registration: request.owner.registration,
        registration_generation: request.owner.generation,
        preparation_nonce: nonce,
        per_transition,
        total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn original_retirement_worker_refuses_foreign_policy_or_missing_entered_scope_then_reuses() {
        use super::super::tests::{external, fixture, release, request};
        use crate::operation_journal::{ParticipantId, WorkUnitRole};
        for foreign in [true, false] {
            fixture(|writer, guard, _| {
                let mut input = request(writer, Duration::from_secs(2));
                let participant = ParticipantId::new();
                // Deliberately invalid private request, solely to test actual producer refusal;
                // it is not a constructor or positive authority/qualification fixture.
                input.retirement = Some(Input {
                    operation: OperationId::new(),
                    work: [
                        WorkUnitKey::new(0, 0, participant, WorkUnitRole::GlobalLibrary),
                        WorkUnitKey::new(0, 2, participant, WorkUnitRole::GlobalLibrary),
                    ],
                    family_digest: [1; 32],
                    acquisition: if foreign {
                        uuid::Uuid::new_v4()
                    } else {
                        uuid::Uuid::parse_str(input.owner.acquisition.as_str()).unwrap()
                    },
                });
                input = if foreign {
                    input.with_lifecycle_policy()
                } else {
                    input.with_wal_binding()
                };
                let result = writer
                    .submit_preparation(input)
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                assert!(
                    matches!(result, Err(GlobalPreparationError::InputBound)),
                    "original private invalid retirement policy was accepted"
                );
                external(guard)
                    .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                    .unwrap();
                let generic = writer
                    .submit_preparation(
                        request(writer, Duration::from_secs(2)).with_lifecycle_policy(),
                    )
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(
                    generic
                        .observation()
                        .wal_scope()
                        .unwrap()
                        .retirement_forward_policy()
                        .is_none()
                );
                release(generic);
                assert!(writer.shutdown());
            });
        }
    }
    #[test]
    fn separate_forward_component_totals_refuse_overflow_and_invalid_units() {
        assert_eq!(
            totals([4096, 8192, 12288], 4096).unwrap(),
            [8192, 16384, 24576]
        );
        for (per, unit) in [
            ([0, 4096, 4096], 4096),
            ([4096; 3], 0),
            ([4097; 3], 4096),
            ([u64::MAX; 3], 1),
            ([i64::MAX as u64; 3], 1),
            ([1, i64::MAX as u64 / 3, i64::MAX as u64 / 3], 1),
        ] {
            assert!(totals(per, unit).is_err());
        }
    }
}
