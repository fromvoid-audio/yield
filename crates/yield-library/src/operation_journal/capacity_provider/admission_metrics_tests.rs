use super::tests::{facts, plan_and_claims};
use super::*;
use crate::operation_journal::capacity_admission_telemetry::{
    CapacityAdmissionDecision as D, tests::Capture,
};

fn fixture(
    free: u64,
) -> (
    CapacityProvider,
    FullCapacityPlan,
    AdmissionClaimInventory,
    VolumeIdentity,
) {
    let volume = VolumeIdentity { device: 801 };
    let (plan, claims) = plan_and_claims(&[(
        volume.clone(),
        FullCapacityAllocationClass::SourceDatabaseCommit,
        4096,
        4096,
    )]);
    let provider = CapacityProvider::qualified_fixture(BTreeMap::from([(
        volume.clone(),
        facts(801, free, 4096),
    )]));
    (provider, plan, claims, volume)
}
#[test]
fn original_provisional_return_pressure_and_drop_keep_original_accounting() {
    let capture = Capture::new();
    let (provider, plan, claims, volume) = fixture(PROTECTED_FREE_SPACE_FLOOR + 4096);
    let token = provider.next_token();
    let lease = provider.admit(&plan, &claims).unwrap();
    assert_eq!(provider.next_token(), token + 1);
    assert_eq!(
        provider.outstanding(&volume, FullCapacityAllocationClass::SourceDatabaseCommit),
        4096
    );
    let (_, second) = plan_and_claims(&[(
        volume.clone(),
        FullCapacityAllocationClass::SourceDatabaseCommit,
        4096,
        4096,
    )]);
    assert!(matches!(
        provider.admit(&plan, &second),
        Err(CapacityProviderError::DiskPressureRecoveryOnly { .. })
    ));
    assert_eq!(provider.next_token(), token + 1);
    assert_eq!(provider.claim_count(), 1);
    let before_drop = capture.snapshot();
    drop(lease);
    assert_eq!(provider.claim_count(), 0);
    assert_eq!(capture.snapshot(), before_drop);
    assert_eq!(before_drop.count(D::ProvisionalLease), 1);
    assert_eq!(before_drop.count(D::Pressure), 1);
    assert_eq!(before_drop.elapsed_buckets.iter().sum::<u64>(), 2);
}
#[test]
fn original_early_and_ledger_refusals_are_counted_once_without_mutation() {
    let capture = Capture::new();
    let (provider, plan, claims, volume) = fixture(PROTECTED_FREE_SPACE_FLOOR);
    let token = provider.next_token();
    assert!(matches!(
        CapacityProvider::unavailable().admit(&plan, &claims),
        Err(CapacityProviderError::Unavailable)
    ));
    assert!(matches!(
        provider.admit(&plan, &claims),
        Err(CapacityProviderError::RecoveryReserveLow { .. })
    ));
    assert!(matches!(
        provider.admit_with_fresh_facts(&plan, &claims, &BTreeMap::new()),
        Err(CapacityProviderError::FactsUnavailable { .. })
    ));
    let wrong = BTreeMap::from([(volume.clone(), facts(801, u64::MAX, 512))]);
    assert!(matches!(
        provider.admit_with_fresh_facts(&plan, &claims, &wrong),
        Err(CapacityProviderError::UnsupportedAllocationUnit { .. })
    ));
    let (_, mismatch) = plan_and_claims(&[(
        volume.clone(),
        FullCapacityAllocationClass::SourceDatabaseCommit,
        8192,
        4096,
    )]);
    assert!(matches!(
        provider.admit(&plan, &mismatch),
        Err(CapacityProviderError::InvalidClaims)
    ));
    {
        let _held = provider.ledger.borrow_mut();
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::LedgerBusy)
        ));
    }
    assert_eq!(provider.next_token(), token);
    assert_eq!(provider.claim_count(), 0);
    provider.ledger.borrow_mut().next_token = u64::MAX;
    assert!(matches!(
        provider.admit(&plan, &claims),
        Err(CapacityProviderError::LeaseTokenExhausted)
    ));
    assert_eq!(provider.next_token(), u64::MAX);
    assert_eq!(provider.claim_count(), 0);
    let snapshot = capture.snapshot();
    for category in [
        D::Unavailable,
        D::ProtectedFloor,
        D::MissingFacts,
        D::UnsupportedFacts,
        D::InvalidInput,
        D::Busy,
        D::Exhausted,
    ] {
        assert_eq!(snapshot.count(category), 1);
    }
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 7);
}
#[test]
fn original_overflow_and_claim_inventory_exhaustion_preserve_held_leases() {
    let capture = Capture::new();
    let (provider, plan, claims, volume) = fixture(u64::MAX);
    // Set the actual retained aggregate boundary; exercise original checked arithmetic.
    provider.ledger.borrow_mut().aggregate_charges.insert(
        ChargeKey {
            volume: volume.clone(),
            allocation_class: FullCapacityAllocationClass::SourceDatabaseCommit,
        },
        u64::MAX,
    );
    let token = provider.next_token();
    assert!(matches!(
        provider.admit(&plan, &claims),
        Err(CapacityProviderError::ArithmeticOverflow)
    ));
    assert_eq!(provider.next_token(), token);
    assert_eq!(provider.claim_count(), 0);
    provider.ledger.borrow_mut().aggregate_charges.clear();
    let mut held = Vec::new();
    for _ in 0..MAX_ADMISSION_CLAIMS {
        let (plan, claims) = plan_and_claims(&[(
            volume.clone(),
            FullCapacityAllocationClass::SourceDatabaseCommit,
            4096,
            4096,
        )]);
        held.push(provider.admit(&plan, &claims).unwrap());
    }
    let full_token = provider.next_token();
    assert!(matches!(
        provider.admit(&plan, &claims),
        Err(CapacityProviderError::ClaimInventoryExhausted)
    ));
    assert_eq!(provider.next_token(), full_token);
    assert_eq!(provider.claim_count(), MAX_ADMISSION_CLAIMS);
    assert_eq!(capture.snapshot().count(D::Overflow), 1);
    assert_eq!(capture.snapshot().count(D::Exhausted), 1);
    assert_eq!(
        capture.snapshot().count(D::ProvisionalLease),
        MAX_ADMISSION_CLAIMS as u64
    );
    let before_drop = capture.snapshot();
    drop(held);
    assert_eq!(provider.claim_count(), 0);
    assert_eq!(capture.snapshot(), before_drop);
}
