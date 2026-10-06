//! Original provider reconstruction and actual strong/explicit Weak custody.
use super::super::tests::{rebuild_claim, rebuild_snapshot};
use super::*;

#[test]
fn original_funded_install_and_exact_replay_match_complete_images() {
    for rows in [0, 1, 259] {
        for released in [false, true] {
            let operation = OperationId::new();
            let volume = VolumeIdentity { device: 17 };
            let claims = (0..rows)
                .map(|_| {
                    rebuild_claim(
                        operation,
                        AdmissionClaimId::new(),
                        volume.clone(),
                        FullCapacityAllocationClass::DestinationStaging,
                        4096,
                        released.then(Uuid::new_v4),
                        false,
                    )
                })
                .collect();
            let image = rebuild_snapshot(claims);
            let input = InputPermit::historical_test_input(1);
            let baseline = input.historical_test_remaining();
            let original = CapacityProvider::unavailable();
            original.install_rebuilt_claims(&image).unwrap();
            let funded = CapacityProvider::unavailable_for_input(&input).unwrap();
            funded
                .install_rebuilt_claims_for_input(&image, &input)
                .unwrap();
            assert_eq!(
                normalized_ledger_image(&original.ledger.borrow()),
                normalized_ledger_image(&funded.ledger.borrow())
            );
            let retained = input.historical_test_remaining();
            funded
                .install_rebuilt_claims_for_input(&image, &input)
                .unwrap();
            assert_eq!(input.historical_test_remaining(), retained);
            drop(funded);
            assert_eq!(input.historical_test_remaining(), baseline);
        }
    }
}

#[test]
fn original_explicit_weak_keeps_header_credit_after_last_strong_drop() {
    let input = InputPermit::historical_test_input(1);
    let baseline = input.historical_test_remaining();
    let funded = CapacityProvider::unavailable_for_input(&input).unwrap();
    let charged = input.historical_test_remaining();
    let weak = funded.mirror.downgrade();
    drop(funded);
    assert!(weak.upgrade().is_none());
    assert_eq!(input.historical_test_remaining(), charged);
    drop(weak);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn original_rc_alias_retains_baseline_and_installed_maps_until_last_holder() {
    let input = InputPermit::historical_test_input(1);
    let baseline = input.historical_test_remaining();
    let funded = CapacityProvider::unavailable_for_input(&input).unwrap();
    let image = rebuild_snapshot(vec![rebuild_claim(
        OperationId::new(),
        AdmissionClaimId::new(),
        VolumeIdentity { device: 9 },
        FullCapacityAllocationClass::DestinationStaging,
        4096,
        None,
        true,
    )]);
    funded
        .install_rebuilt_claims_for_input(&image, &input)
        .unwrap();
    let charged = input.historical_test_remaining();
    let alias = funded.ledger.clone();
    drop(funded);
    assert_eq!(alias.borrow().charges_by_token.len(), 1);
    assert_eq!(input.historical_test_remaining(), charged);
    drop(alias);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn original_strict_header_pressure_refuses_before_provider_allocation() {
    let input = InputPermit::historical_test_input(1);
    let baseline = input.historical_test_remaining();
    let hold = input
        .reserve_historical_scratch(baseline - header_bytes() + 1)
        .unwrap();
    assert!(matches!(
        CapacityProvider::unavailable_for_input(&input),
        Err(CapacityProviderError::RebuildResourceBound)
    ));
    assert_eq!(input.historical_test_remaining(), header_bytes() - 1);
    drop(hold);
    assert_eq!(input.historical_test_remaining(), baseline);
}
