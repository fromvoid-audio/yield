//! Tag3 is an explicit frozen storage policy, never an original owner or physical grant.
use super::*;
use crate::operation_journal::{
    WorkUnitKey, WorkUnitRole, source_registry_retirement::SourceRegistryRetirementIntentArgs,
};

pub(super) fn validate_shape(inventory: &DatabaseClaimInventory) -> Result<()> {
    require(
        inventory.bindings.len() == 2 && inventory.slices.len() == 6,
        "retirement policy exact counts",
    )?;
    for role in [DatabaseRole::Source, DatabaseRole::Global] {
        let mut bindings = inventory.bindings.iter().filter(|b| b.role == role);
        let binding = bindings
            .next()
            .ok_or(DatabaseClaimError("retirement policy missing role"))?;
        require(
            bindings.next().is_none() && binding.participants.len() == 1,
            "retirement policy exact participants",
        )?;
        let participant = &binding.participants[0];
        require(
            participant.requirement == 1
                && participant.revision_domain
                    == if role == DatabaseRole::Source {
                        RevisionDomain::SourceLeaseLifecycle
                    } else {
                        RevisionDomain::GlobalMutationGeneration
                    },
            "retirement policy role/domain",
        )?;
        let selected = inventory.slices.iter().filter(|s| s.binding == binding.id);
        let mut main = 0;
        let mut side = 0;
        for slice in selected {
            require(
                slice.participant == participant.id
                    && slice.direction
                        == if role == DatabaseRole::Source {
                            ClaimDirection::LeaseMaintenance
                        } else {
                            ClaimDirection::RequiredForward
                        },
                "retirement policy direction/participant",
            )?;
            match (role, slice.class) {
                (DatabaseRole::Source, FullCapacityAllocationClass::SourceDatabaseCommit)
                | (DatabaseRole::Global, FullCapacityAllocationClass::GlobalDatabaseCommit) => {
                    main += 1
                }
                (DatabaseRole::Source, FullCapacityAllocationClass::SourceWalShm)
                | (DatabaseRole::Global, FullCapacityAllocationClass::GlobalWalShm) => {
                    require(
                        slice.wal_ceiling > 0 && slice.shm_ceiling > 0,
                        "retirement policy sidecar components",
                    )?;
                    side += 1;
                }
                _ => return Err(DatabaseClaimError("retirement policy class")),
            }
        }
        let expected = if role == DatabaseRole::Source { 1 } else { 2 };
        require(
            main == expected && side == expected,
            "retirement policy component pairs",
        )?;
    }
    Ok(())
}

/// Reconstruct immutable comparison data only. Each exact work/component gets a distinct ID;
/// neither this hash nor a matching stored row can issue an owner decision or permit.
pub(crate) fn slice_identity(
    header: &InventoryHeader,
    binding: &DatabaseBinding,
    claim: Uuid,
    family_digest: &[u8; 32],
    work: WorkUnitKey,
    direction: ClaimDirection,
    class: FullCapacityAllocationClass,
) -> Result<Uuid> {
    let role = match work.role() {
        WorkUnitRole::GlobalLibrary => 1,
        WorkUnitRole::SourceLeaseMaintenance => 2,
        _ => return Err(DatabaseClaimError("retirement work role")),
    };
    let mut hash = blake3::Hasher::new_derive_key("yield.retirement-forward-slice.v1");
    hash.update(header.decision_nonce.as_bytes());
    hash.update(&header.ownership_generation.to_le_bytes());
    hash.update(binding.id.as_bytes());
    hash.update(binding.registration.as_bytes());
    for integer in [
        binding.volume,
        binding.allocation_unit,
        binding.owner_generation,
        binding.lifecycle,
        binding.current_revision,
    ] {
        hash.update(&integer.to_le_bytes());
    }
    for text in [
        &header.profile,
        &header.acquisition,
        &binding.main_file,
        &binding.database_root,
        &binding.root_binding,
    ] {
        identity(text)?;
        hash.update(&(text.len() as u16).to_le_bytes());
        hash.update(text.as_bytes());
    }
    hash.update(claim.as_bytes());
    hash.update(family_digest);
    hash.update(&[
        binding.role as u8,
        direction as u8,
        class_tag(class)?,
        role,
        work.action_ordinal(),
    ]);
    hash.update(&work.step_ordinal().to_le_bytes());
    hash.update(work.participant_id().as_uuid().as_bytes());
    let id = Uuid::from_bytes(
        hash.finalize().as_bytes()[..16]
            .try_into()
            .expect("fixed identity digest"),
    );
    uuid(id)?;
    Ok(id)
}

/// Distinct historical domain: changing the serial changes every work/component identity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn registered_slice_identity(
    header: &InventoryHeader,
    binding: &DatabaseBinding,
    source_registration_generation: u64,
    claim: Uuid,
    family_digest: &[u8; 32],
    work: WorkUnitKey,
    direction: ClaimDirection,
    class: FullCapacityAllocationClass,
) -> Result<Uuid> {
    integer(source_registration_generation, true)?;
    let prior = slice_identity(
        header,
        binding,
        claim,
        family_digest,
        work,
        direction,
        class,
    )?;
    let mut hash = blake3::Hasher::new_derive_key("yield.registered-retirement-forward-slice.v1");
    hash.update(&source_registration_generation.to_le_bytes());
    hash.update(prior.as_bytes());
    let id = Uuid::from_bytes(
        hash.finalize().as_bytes()[..16]
            .try_into()
            .expect("fixed digest"),
    );
    uuid(id)?;
    Ok(id)
}

pub(super) fn validate_record(
    inventory: &DatabaseClaimInventory,
    record: &super::super::codec::StoredOperationRecord,
) -> Result<()> {
    require(
        record.schema_version == super::super::model::SCHEMA_V18
            && record.intent.kind == super::super::model::OperationKind::SourceRegistryRetirement,
        "retirement policy family/version",
    )?;
    let Some(super::super::coordinator::UniversalCommandArgs::SourceRegistryRetirement(args)) =
        record.universal_command_args()
    else {
        return Err(DatabaseClaimError("retirement policy frozen family"));
    };
    args.validate()
        .map_err(|_| DatabaseClaimError("retirement policy frozen shape"))?;
    require(
        args.operation_id()
            .map_err(|_| DatabaseClaimError("retirement policy operation"))?
            == record.operation_id,
        "retirement policy exact operation",
    )?;
    let digest = args
        .initial_forward_policy_digest()
        .map_err(|_| DatabaseClaimError("retirement policy family digest"))?;
    validate_work_pairs(inventory, args, &digest)
}
fn validate_work_pairs(
    inventory: &DatabaseClaimInventory,
    args: &SourceRegistryRetirementIntentArgs,
    digest: &[u8; 32],
) -> Result<()> {
    let mut matched = 0;
    let mut prior_global = None;
    for work in [
        args.source_work(),
        args.draining_work(),
        args.retired_work(),
    ] {
        let source = work.role() == WorkUnitRole::SourceLeaseMaintenance;
        let role = if source {
            DatabaseRole::Source
        } else {
            DatabaseRole::Global
        };
        let binding = inventory
            .bindings
            .iter()
            .find(|b| b.role == role)
            .ok_or(DatabaseClaimError("retirement policy binding"))?;
        require(
            binding.participants[0].id == work.participant_id().as_uuid(),
            "retirement policy work participant",
        )?;
        let direction = if source {
            ClaimDirection::LeaseMaintenance
        } else {
            ClaimDirection::RequiredForward
        };
        let classes = if source {
            [
                FullCapacityAllocationClass::SourceDatabaseCommit,
                FullCapacityAllocationClass::SourceWalShm,
            ]
        } else {
            [
                FullCapacityAllocationClass::GlobalDatabaseCommit,
                FullCapacityAllocationClass::GlobalWalShm,
            ]
        };
        let mut ceilings = [0; 3];
        for (index, class) in classes.into_iter().enumerate() {
            let mut selected = None;
            for slice in inventory
                .slices
                .iter()
                .filter(|s| s.binding == binding.id && s.class == class)
            {
                let expected = if let Some(generation) = inventory.source_registration_generation()
                {
                    registered_slice_identity(
                        &inventory.header,
                        binding,
                        generation,
                        slice.claim,
                        digest,
                        work,
                        direction,
                        class,
                    )?
                } else {
                    slice_identity(
                        &inventory.header,
                        binding,
                        slice.claim,
                        digest,
                        work,
                        direction,
                        class,
                    )?
                };
                if slice.id == expected {
                    require(selected.is_none(), "retirement policy duplicate work slice")?;
                    selected = Some(slice);
                }
            }
            let slice = selected.ok_or(DatabaseClaimError(
                "retirement policy omitted/mixed work slice",
            ))?;
            if index == 0 {
                ceilings[0] = slice.ceiling;
            } else {
                ceilings[1] = slice.wal_ceiling;
                ceilings[2] = slice.shm_ceiling;
            }
            matched += 1;
        }
        if !source {
            if let Some(prior) = prior_global {
                require(
                    prior == ceilings,
                    "retirement policy separate equal full component budgets",
                )?;
            }
            prior_global = Some(ceilings);
        }
    }
    require(
        matched == inventory.slices.len(),
        "retirement policy surplus work slices",
    )
}
