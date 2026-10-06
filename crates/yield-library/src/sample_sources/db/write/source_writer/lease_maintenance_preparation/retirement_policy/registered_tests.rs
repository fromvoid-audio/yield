//! Genuine original Source preparation/custody; historical proposals qualify no transitions.
use super::*;
use crate::operation_journal::{
    OperationId, ParticipantId, WorkUnitKey, WorkUnitRole,
    source_registry_retirement::SourceRegistryRetirementIntentArgs,
};
use crate::sample_sources::db::{
    SourceRegistryReferenceExpected, SourceRegistryReferenceProposal,
    SourceRegistryReferenceTransition as Transition,
};

fn frozen(
    writer: SourceWriter,
    registration: &SourceAuthorityRegistration,
) -> (
    SourceWriter,
    SourceRegistryRetirementIntentArgs,
    SourceLeaseMaintenanceBinding,
) {
    let reference = writer
        .capture_registry_reference_observation(Some(registration))
        .unwrap();
    let held = prepare(writer);
    let binding = held.capture_lease_binding(registration).unwrap();
    let fence = binding.stored_fence().clone();
    let writer = held.abort().unwrap();
    let source = ParticipantId::new();
    let global = ParticipantId::new();
    let operation = OperationId::new();
    // Expected row identities are historical comparison fixtures, never global receipts.
    let draining = SourceRegistryReferenceProposal::new(
        &reference,
        operation,
        WorkUnitKey::new(0, 0, global, WorkUnitRole::GlobalLibrary),
        Transition::Draining,
        SourceRegistryReferenceExpected::Present {
            state: Transition::Enrolled,
            row_identity: [1; 32],
        },
    )
    .unwrap();
    let retired = SourceRegistryReferenceProposal::new(
        &reference,
        operation,
        WorkUnitKey::new(0, 2, global, WorkUnitRole::GlobalLibrary),
        Transition::Retired,
        SourceRegistryReferenceExpected::Present {
            state: Transition::Draining,
            row_identity: [2; 32],
        },
    )
    .unwrap();
    let args = SourceRegistryRetirementIntentArgs::new(source, global, fence, &draining, &retired)
        .unwrap();
    (writer, args, binding)
}
fn prepare_retirement(
    writer: SourceWriter,
    args: &SourceRegistryRetirementIntentArgs,
) -> SourceLeaseMaintenancePreparation {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    input.bind_retirement_policy(args).unwrap();
    writer.prepare_lease_maintenance(input, deadline).unwrap()
}

#[test]
fn source_retirement_policy_requires_original_registered_use_and_exact_encoded_scope() {
    let (_fixture, writer) = fixture();
    let (registration, _proof, request) = registered(&writer.database);
    let (writer, args, prior_binding) = frozen(writer, &registration);
    let reference = writer
        .capture_registry_reference_observation(Some(&registration))
        .unwrap();
    let other_operation = OperationId::new();
    let global = args.draining_work().participant_id();
    let other_draining = SourceRegistryReferenceProposal::new(
        &reference,
        other_operation,
        args.draining_work(),
        Transition::Draining,
        SourceRegistryReferenceExpected::Present {
            state: Transition::Enrolled,
            row_identity: [1; 32],
        },
    )
    .unwrap();
    let other_retired = SourceRegistryReferenceProposal::new(
        &reference,
        other_operation,
        args.retired_work(),
        Transition::Retired,
        SourceRegistryReferenceExpected::Present {
            state: Transition::Draining,
            row_identity: [2; 32],
        },
    )
    .unwrap();
    let other_family = SourceRegistryRetirementIntentArgs::new(
        args.source_participant(),
        global,
        args.lease_fence().clone(),
        &other_draining,
        &other_retired,
    )
    .unwrap();
    let other_work = SourceRegistryRetirementIntentArgs::new(
        ParticipantId::new(),
        global,
        args.lease_fence().clone(),
        &other_draining,
        &other_retired,
    )
    .unwrap();
    let initial_settings = settings(&writer);
    let outside = external(&writer);
    let initial_lease = lease(&outside);
    let initial_changes = writer.database.connection.total_changes();
    let mut held = prepare_retirement(writer, &args);
    let mut binding = held.capture_lease_binding(&registration).unwrap();
    let bytes = binding.stored_fence().canonical_bytes().unwrap();
    assert_ne!(
        binding.stored_fence().preparation_nonce(),
        prior_binding.stored_fence().preparation_nonce()
    );
    assert!(
        held.capture_retirement_forward_policy(&mut binding, &registration, &bytes)
            .is_err()
    );
    assert!(binding.retirement_forward_policy().is_none());
    assert_eq!(count(&request), 0);
    held.retain_registered_lease_binding(&binding, &registration)
        .unwrap();
    assert_eq!(count(&request), 1);
    let mut changed = bytes.clone();
    changed[0] ^= 1;
    assert!(
        held.capture_retirement_forward_policy(&mut binding, &registration, &changed)
            .is_err()
    );
    assert!(binding.retirement_forward_policy().is_none());
    assert_eq!(count(&request), 1);
    held.capture_retirement_forward_policy(&mut binding, &registration, &bytes)
        .unwrap();
    let policy = binding.retirement_forward_policy().unwrap();
    assert!(policy.matches_original(
        &args,
        &args.initial_forward_policy_digest().unwrap(),
        binding.stored_fence(),
        &bytes
    ));
    for foreign in [&other_family, &other_work] {
        foreign.validate().unwrap();
        assert!(!policy.matches_original(
            foreign,
            &foreign.initial_forward_policy_digest().unwrap(),
            binding.stored_fence(),
            &bytes,
        ));
    }
    assert_eq!(
        policy.component_ceilings(),
        binding.stored_fence().write_ceilings()
    );
    assert!(!policy.matches_original(
        &args,
        &args.initial_forward_policy_digest().unwrap(),
        binding.stored_fence(),
        &changed
    ));
    assert!(!policy.matches_original(
        &args,
        &args.initial_forward_policy_digest().unwrap(),
        prior_binding.stored_fence(),
        &prior_binding.stored_fence().canonical_bytes().unwrap()
    ));
    assert!(
        held.capture_retirement_forward_policy(&mut binding, &registration, &bytes)
            .is_err()
    );
    assert_eq!(
        held.custody.writer().database.connection.total_changes(),
        initial_changes
    );
    drop(binding);
    drop(prior_binding);
    let writer = held.abort().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(settings(&writer), initial_settings);
    assert_eq!(lease(&outside), initial_lease);
    writer
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
}

#[test]
fn source_retirement_policy_refuses_prior_genuine_preparation_and_keeps_current_custody() {
    let (_fixture, writer) = fixture();
    let (registration, _proof, request) = registered(&writer.database);
    let (writer, args, mut prior_binding) = frozen(writer, &registration);
    let initial_settings = settings(&writer);
    let mut held = prepare_retirement(writer, &args);
    let mut current = retain(&mut held, &registration);
    let old_bytes = prior_binding.stored_fence().canonical_bytes().unwrap();
    assert!(
        held.capture_retirement_forward_policy(&mut prior_binding, &registration, &old_bytes)
            .is_err()
    );
    assert!(prior_binding.retirement_forward_policy().is_none());
    assert_eq!(count(&request), 1);
    let current_bytes = current.stored_fence().canonical_bytes().unwrap();
    held.capture_retirement_forward_policy(&mut current, &registration, &current_bytes)
        .unwrap();
    assert!(current.retirement_forward_policy().is_some());
    drop(current);
    drop(prior_binding);
    let writer = held.abort().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(settings(&writer), initial_settings);
    writer
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
}

#[test]
fn generic_registered_lease_preparation_retains_no_retirement_policy() {
    let (_fixture, writer) = fixture();
    let (registration, _proof, request) = registered(&writer.database);
    let mut held = prepare(writer);
    let mut binding = retain(&mut held, &registration);
    let bytes = binding.stored_fence().canonical_bytes().unwrap();
    held.capture_retirement_forward_policy(&mut binding, &registration, &bytes)
        .unwrap();
    assert!(binding.retirement_forward_policy().is_none());
    assert_eq!(count(&request), 1);
    drop(binding);
    let writer = held.abort().unwrap();
    assert_eq!(count(&request), 0);
    writer
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
}
