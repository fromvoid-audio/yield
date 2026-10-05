//! Original actor-ledger tests; fixtures never qualify native capacity or publication.
use super::super::tests::{plan_and_claims, rebuild_claim, rebuild_snapshot};
use super::tests::{admit, provider};
use super::*;
const CLASS: FullCapacityAllocationClass = FullCapacityAllocationClass::DestinationStaging;

fn durable(provider: &CapacityProvider) -> (OperationId, AdmissionClaim, ProvisionalCapacityLease) {
    let (plan, claims) = plan_and_claims(&[(VolumeIdentity { device: 1 }, CLASS, 4096, 4096)]);
    let claim = claims.claims()[0].clone();
    let mut lease = provider.admit(&plan, &claims).unwrap();
    let operation = OperationId::new();
    lease.mark_durable(operation).unwrap();
    (operation, claim, lease)
}

#[test]
fn exact_durable_charge_binding_does_not_admit_or_release_bytes() {
    let provider = provider(&[1]);
    let (operation, claim, lease) = durable(&provider);
    let before = provider.ledger.borrow().aggregate_charges.clone();
    let binding = provider.bind_publication_charge(operation, &claim).unwrap();
    assert!(binding.is_current());
    provider.validate_publication_charge(&binding).unwrap();
    assert_eq!(binding.claim(), &claim);
    assert_eq!(binding.operation(), operation);
    assert_eq!(binding.with_current(|v| v.incremental_bytes), Some(4096));
    drop(binding);
    drop(lease);
    assert_eq!(provider.ledger.borrow().aggregate_charges, before);
}

#[test]
fn same_volume_or_amount_never_substitutes_for_exact_operation_claim_class_and_bytes() {
    let provider = provider(&[1, 2]);
    let (operation, claim, _lease) = durable(&provider);
    assert!(
        provider
            .bind_publication_charge(OperationId::new(), &claim)
            .is_err()
    );
    for wrong in [
        AdmissionClaim::new(AdmissionClaimId::new(), claim.volume().clone(), CLASS, 4096).unwrap(),
        AdmissionClaim::new(claim.claim_id(), VolumeIdentity { device: 2 }, CLASS, 4096).unwrap(),
        AdmissionClaim::new(
            claim.claim_id(),
            claim.volume().clone(),
            FullCapacityAllocationClass::JournalRecord,
            4096,
        )
        .unwrap(),
        AdmissionClaim::new(claim.claim_id(), claim.volume().clone(), CLASS, 8192).unwrap(),
    ] {
        assert!(provider.bind_publication_charge(operation, &wrong).is_err());
    }
    assert_eq!(
        provider
            .ledger
            .borrow()
            .aggregate_charges
            .values()
            .sum::<u64>(),
        4096
    );
}

#[test]
fn provisional_audit_pending_and_duplicate_tokens_cannot_mint_binding() {
    let provider = provider(&[1]);
    let (plan, claims) = plan_and_claims(&[(VolumeIdentity { device: 1 }, CLASS, 4096, 4096)]);
    let claim = &claims.claims()[0];
    let operation = OperationId::new();
    let mut lease = provider.admit(&plan, &claims).unwrap();
    assert!(provider.bind_publication_charge(operation, claim).is_err());
    lease.pin_for_persistence(operation).unwrap();
    assert!(provider.bind_publication_charge(operation, claim).is_err());
    lease.mark_durable(operation).unwrap();
    assert!(provider.bind_publication_charge(operation, claim).is_ok());
    // Exercise defensive ambiguity refusal on the real ledger, rather than a grant model.
    let duplicate = provider
        .ledger
        .borrow()
        .charges_by_token
        .values()
        .next()
        .unwrap()
        .clone();
    provider
        .ledger
        .borrow_mut()
        .charges_by_token
        .insert(u64::MAX, duplicate);
    assert!(matches!(
        provider.bind_publication_charge(operation, claim),
        Err(CapacityProviderError::ClaimReleaseAmbiguous)
    ));
}

#[test]
fn ordinary_release_and_unrelated_admission_invalidate_original_generation() {
    let provider = provider(&[1, 2]);
    let (operation, claim, _lease) = durable(&provider);
    let binding = provider.bind_publication_charge(operation, &claim).unwrap();
    let other = admit(&provider, 2, CLASS);
    assert!(!binding.is_current());
    assert!(provider.validate_publication_charge(&binding).is_err());
    let current = provider.bind_publication_charge(operation, &claim).unwrap();
    provider
        .release_claim(&OrdinaryClaimReleaseReceipt::for_test(
            operation,
            claim.claim_id(),
            Uuid::new_v4(),
            claim.volume().clone(),
            CLASS,
            4096,
        ))
        .unwrap();
    assert!(!current.is_current());
    assert!(provider.bind_publication_charge(operation, &claim).is_err());
    drop(other);
}

#[test]
fn restart_charge_binding_is_original_provider_specific_and_provider_loss_closes_it() {
    let operation = OperationId::new();
    let claim = AdmissionClaim::new(
        AdmissionClaimId::new(),
        VolumeIdentity { device: 1 },
        CLASS,
        8192,
    )
    .unwrap();
    let image = rebuild_snapshot(vec![rebuild_claim(
        operation,
        claim.claim_id(),
        claim.volume().clone(),
        CLASS,
        8192,
        None,
        false,
    )]);
    let first = CapacityProvider::unavailable();
    let second = CapacityProvider::unavailable();
    first.install_rebuilt_claims(&image).unwrap();
    second.install_rebuilt_claims(&image).unwrap();
    let binding = first.bind_publication_charge(operation, &claim).unwrap();
    first.validate_publication_charge(&binding).unwrap();
    assert!(second.validate_publication_charge(&binding).is_err());
    drop(first);
    assert!(!binding.is_current());
    assert!(second.validate_publication_charge(&binding).is_err());
}

#[test]
fn unknown_original_mutation_permanently_closes_exact_binding() {
    let provider = provider(&[1]);
    let (operation, claim, _lease) = durable(&provider);
    let binding = provider.bind_publication_charge(operation, &claim).unwrap();
    let mutation = ChargeMutation::begin(&provider.ledger.borrow());
    assert!(!binding.is_current());
    assert!(provider.bind_publication_charge(operation, &claim).is_err());
    drop(mutation);
    assert!(!binding.is_current());
    assert!(provider.bind_publication_charge(operation, &claim).is_err());
}
