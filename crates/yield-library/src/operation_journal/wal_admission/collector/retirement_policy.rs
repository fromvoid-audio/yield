//! Six original initial-forward requirements. Comparison never grants physical authority.
use super::*;
use crate::operation_journal::{
    AdmissionClaimInventory, FullCapacityPlan, ParticipantSet, WorkUnitKey, WorkUnitRole,
    database_claims::ClaimDirection, full_capacity::FullCapacityAllocationClass as Class,
    source_registry_retirement::SourceRegistryRetirementIntentArgs,
};

#[derive(Clone, Copy)]
struct Slice {
    work: WorkUnitKey,
    direction: ClaimDirection,
    class: Class,
    device: u64,
    unit: u64,
    ceiling: u64,
    wal: u64,
    shm: u64,
}
fn pair(
    work: WorkUnitKey,
    device: u64,
    unit: u64,
    per: [u64; 3],
) -> Result<[Slice; 2], WalPreparationError> {
    let bad = || WalPreparationError::InvalidInput;
    if unit == 0
        || per
            .iter()
            .any(|n| *n == 0 || *n > i64::MAX as u64 || *n % unit != 0)
    {
        return Err(bad());
    }
    let (direction, main, side) = match work.role() {
        WorkUnitRole::SourceLeaseMaintenance => (
            ClaimDirection::LeaseMaintenance,
            Class::SourceDatabaseCommit,
            Class::SourceWalShm,
        ),
        WorkUnitRole::GlobalLibrary => (
            ClaimDirection::RequiredForward,
            Class::GlobalDatabaseCommit,
            Class::GlobalWalShm,
        ),
        _ => return Err(bad()),
    };
    let total = per[1]
        .checked_add(per[2])
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(bad)?;
    Ok([
        Slice {
            work,
            direction,
            class: main,
            device,
            unit,
            ceiling: per[0],
            wal: 0,
            shm: 0,
        },
        Slice {
            work,
            direction,
            class: side,
            device,
            unit,
            ceiling: total,
            wal: per[1],
            shm: per[2],
        },
    ])
}

fn compare(
    slices: &[Slice; 6],
    plan: &FullCapacityPlan,
    claims: &AdmissionClaimInventory,
) -> Result<(), WalPreparationError> {
    let bad = || WalPreparationError::InvalidInput;
    // Preserve all three distinct work pairs and both complete sequential global
    // component policies; an equal aggregate cannot hide a changed WAL/SHM split.
    if slices[0].work != slices[1].work
        || slices[2].work != slices[3].work
        || slices[4].work != slices[5].work
        || slices[2].work == slices[4].work
        || slices[2].ceiling != slices[4].ceiling
        || (slices[3].ceiling, slices[3].wal, slices[3].shm)
            != (slices[5].ceiling, slices[5].wal, slices[5].shm)
    {
        return Err(bad());
    }
    let database = |class| {
        matches!(
            class,
            Class::SourceDatabaseCommit
                | Class::SourceWalShm
                | Class::GlobalDatabaseCommit
                | Class::GlobalWalShm
        )
    };
    if plan
        .allocations()
        .iter()
        .filter(|a| database(a.class()))
        .count()
        != 4
        || claims
            .claims()
            .iter()
            .filter(|c| database(c.allocation_class()))
            .count()
            != 4
    {
        return Err(bad());
    }
    for slice in slices {
        let source = slice.work.role() == WorkUnitRole::SourceLeaseMaintenance;
        if !matches!(
            slice.work.role(),
            WorkUnitRole::SourceLeaseMaintenance | WorkUnitRole::GlobalLibrary
        ) || slice.direction
            != if source {
                ClaimDirection::LeaseMaintenance
            } else {
                ClaimDirection::RequiredForward
            }
            || !matches!(
                (source, slice.class),
                (true, Class::SourceDatabaseCommit | Class::SourceWalShm)
                    | (false, Class::GlobalDatabaseCommit | Class::GlobalWalShm)
            )
            || match slice.class {
                Class::SourceDatabaseCommit | Class::GlobalDatabaseCommit => {
                    slice.wal != 0 || slice.shm != 0
                }
                _ => {
                    slice.wal == 0
                        || slice.shm == 0
                        || slice.wal.checked_add(slice.shm) != Some(slice.ceiling)
                }
            }
        {
            return Err(bad());
        }
        let total = slices
            .iter()
            .filter(|s| s.class == slice.class)
            .try_fold(0u64, |total, next| {
                if next.device != slice.device || next.unit != slice.unit {
                    return None;
                }
                total
                    .checked_add(next.ceiling)
                    .filter(|n| *n <= i64::MAX as u64)
            })
            .ok_or_else(bad)?;
        let allocation = plan
            .allocations()
            .iter()
            .find(|a| a.class() == slice.class)
            .ok_or_else(bad)?;
        let claim = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == slice.class)
            .ok_or_else(bad)?;
        if allocation.volume().device != slice.device
            || allocation.allocation_unit() != slice.unit
            || allocation.allocated_bytes() != total
            || claim.volume() != allocation.volume()
            || claim.allocated_bytes() != total
        {
            return Err(bad());
        }
    }
    claims.validate_against(plan).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, FullCapacityAllocation, ParticipantId,
    };

    #[test]
    fn retirement_component_pairs_refuse_zero_misalignment_and_checked_overflow() {
        // Pure logical arithmetic only: these work keys grant no owner or qualification.
        let work = WorkUnitKey::new(
            0,
            1,
            ParticipantId::new(),
            WorkUnitRole::SourceLeaseMaintenance,
        );
        for (unit, per) in [
            (0, [1, 1, 1]),
            (2, [1, 2, 2]),
            (1, [0, 1, 1]),
            (1, [1, i64::MAX as u64, 1]),
            (1, [u64::MAX, 1, 1]),
        ] {
            assert!(pair(work, 1, unit, per).is_err());
        }
        assert!(pair(work, 1, 4096, [4096, 8192, 4096]).is_ok());
    }

    #[test]
    fn retirement_six_slices_preserve_work_components_and_exact_claim_conservation() {
        // Logical comparison fixture; production obtains every ceiling from original actors.
        let source = pair(
            WorkUnitKey::new(
                0,
                1,
                ParticipantId::new(),
                WorkUnitRole::SourceLeaseMaintenance,
            ),
            42,
            4096,
            [4096, 8192, 4096],
        )
        .unwrap();
        let global = ParticipantId::new();
        let draining = pair(
            WorkUnitKey::new(0, 0, global, WorkUnitRole::GlobalLibrary),
            42,
            4096,
            [8192, 4096, 8192],
        )
        .unwrap();
        let retired = pair(
            WorkUnitKey::new(0, 2, global, WorkUnitRole::GlobalLibrary),
            42,
            4096,
            [8192, 4096, 8192],
        )
        .unwrap();
        let slices = [
            source[0],
            source[1],
            draining[0],
            draining[1],
            retired[0],
            retired[1],
        ];
        let totals = [
            (Class::SourceDatabaseCommit, 4096),
            (Class::SourceWalShm, 12288),
            (Class::GlobalDatabaseCommit, 16384),
            (Class::GlobalWalShm, 24576),
        ];
        let plan = FullCapacityPlan::new(
            totals
                .into_iter()
                .map(|(class, bytes)| {
                    FullCapacityAllocation::new(
                        VolumeIdentity { device: 42 },
                        4096,
                        class,
                        bytes,
                        bytes,
                        PROTECTED_FREE_SPACE_FLOOR,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let claims = AdmissionClaimInventory::new(
            totals
                .into_iter()
                .map(|(class, bytes)| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        VolumeIdentity { device: 42 },
                        class,
                        bytes,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        compare(&slices, &plan, &claims).unwrap();
        for fault in 0..5 {
            let mut other = slices;
            match fault {
                0 => {
                    other[3].wal += 4096;
                    other[3].shm -= 4096;
                }
                1 => {
                    other[4].work = other[2].work;
                    other[5].work = other[3].work;
                }
                2 => other[0].ceiling += 4096,
                3 => other[1].device += 1,
                _ => other[1].direction = ClaimDirection::RequiredCompensation,
            }
            assert!(
                compare(&other, &plan, &claims).is_err(),
                "comparison fault {fault}"
            );
        }
    }
}

impl Collector {
    pub(in crate::operation_journal) fn validate_retirement_policy(
        &mut self,
        args: &SourceRegistryRetirementIntentArgs,
        participants: &ParticipantSet,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        digest: [u8; 32],
        ready: &super::super::OriginalReadyRound,
    ) -> Result<(), WalPreparationError> {
        // A foreign envelope cannot consume or cancel another original preparation.
        if digest != self.input.digest {
            return Err(WalPreparationError::InvalidInput);
        }
        let result =
            self.compare_retirement_policy(args, participants, plan, claims, digest, ready);
        if let Err(error) = &result {
            // Matching original input with no admissible partition is definite NoAttempt.
            self.refuse_decision(error.clone());
        }
        result
    }

    fn compare_retirement_policy(
        &self,
        args: &SourceRegistryRetirementIntentArgs,
        participants: &ParticipantSet,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        digest: [u8; 32],
        ready: &super::super::OriginalReadyRound,
    ) -> Result<(), WalPreparationError> {
        let bad = || WalPreparationError::InvalidInput;
        args.validate_against_participants(participants)
            .map_err(|_| bad())?;
        if digest != self.input.digest
            || self.reason.is_some()
            || self.done
            || self.shared.canceled.load(Ordering::Acquire)
            || Instant::now() >= self.input.deadline
        {
            return Err(bad());
        }
        ready.require_provenance(self.current.as_ref().ok_or_else(bad)?)?;
        let snapshot = self.snapshot.as_ref().ok_or_else(bad)?;
        if snapshot.databases().len() != 2 || self.held.len() != 2 {
            return Err(bad());
        }
        let mut global = None;
        let mut lease = None;
        for database in snapshot.databases() {
            match database {
                WalPreparedDatabase::Global(view) if global.is_none() => global = Some(view),
                WalPreparedDatabase::Lease(view) if lease.is_none() => lease = Some(view),
                _ => return Err(bad()),
            }
        }
        let global = global.ok_or_else(bad)?;
        let lease = lease.ok_or_else(bad)?;
        let fence = lease.original_binding().stored_fence();
        fence.validate().map_err(|_| bad())?;
        if !lease.registered_custody_was_retained()
            || !fence.matches_initial_policy(args.lease_fence())
            || fence.subject().profile_acquisition() != self.profile_acquisition
            || uuid::Uuid::parse_str(global.acquisition().as_str()).ok()
                != Some(self.profile_acquisition)
            || global.command_digest() != &digest
            || !global.lifecycle_policy_was_entered()
            || fence.subject().database_identity() == global.database_identity()
        {
            return Err(bad());
        }
        let scope = global.wal_scope().ok_or_else(bad)?;
        let allocation = global.lifecycle_allocation().ok_or_else(bad)?;
        let policy = scope.retirement_forward_policy().ok_or_else(bad)?;
        if !policy.matches_original(args, global.owner(), global.nonce(), allocation)
            || scope.main().identity() != global.database_identity()
            || scope.root_identity().is_empty()
            || [scope.main(), scope.wal(), scope.shm()]
                .into_iter()
                .zip([allocation.main(), allocation.wal(), allocation.shm()])
                .any(|(actual, expected)| {
                    actual.identity() != expected.identity()
                        || actual.device() != expected.device()
                        || actual.allocation_unit() != expected.allocation_unit()
                        || actual.observed_bytes() != expected.observed_bytes()
                })
        {
            return Err(bad());
        }
        let source_main = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == Class::SourceDatabaseCommit)
            .ok_or_else(bad)?;
        let source_side = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == Class::SourceWalShm)
            .ok_or_else(bad)?;
        let expected = SourceWalTransportCorrelation::new(
            digest,
            args.source_participant(),
            source_main.claim_id(),
            source_side.claim_id(),
        )
        .map_err(|_| bad())?;
        if lease.correlation() != &expected {
            return Err(bad());
        }
        let source_policy = lease
            .original_binding()
            .retirement_forward_policy()
            .ok_or_else(bad)?;
        if !source_policy.matches_original(
            args,
            policy.family_digest(),
            fence,
            lease.scope_descriptor_bytes(),
        ) {
            return Err(bad());
        }
        let source = pair(
            args.source_work(),
            fence.device(),
            fence.allocation_unit(),
            source_policy.component_ceilings(),
        )?;
        let draining = pair(
            args.draining_work(),
            scope.main().device(),
            scope.main().allocation_unit(),
            policy.per_transition_ceilings(),
        )?;
        let retired = pair(
            args.retired_work(),
            scope.main().device(),
            scope.main().allocation_unit(),
            policy.per_transition_ceilings(),
        )?;
        compare(
            &[
                source[0],
                source[1],
                draining[0],
                draining[1],
                retired[0],
                retired[1],
            ],
            plan,
            claims,
        )
    }
}
