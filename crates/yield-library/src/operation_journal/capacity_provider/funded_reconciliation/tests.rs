use super::*;

fn image(rows: usize, released: bool, mixed: bool) -> OrdinaryClaimRebuildSnapshot {
    let operations = [OperationId::new(), OperationId::new(), OperationId::new()];
    OrdinaryClaimRebuildSnapshot::for_test(
        (0..rows)
            .map(|index| {
                RebuildClaim::for_test(
                    operations[index % operations.len()],
                    AdmissionClaimId::new(),
                    VolumeIdentity {
                        device: 1 + (index % 2) as u64,
                    },
                    FullCapacityAllocationClass::DestinationStaging,
                    1,
                    (released || (mixed && index % 3 == 0)).then(Uuid::new_v4),
                    mixed && index % 5 == 0,
                )
            })
            .collect(),
        CapacityRebuildReadiness::ready(),
    )
}
fn ledger_image(
    provider: &CapacityProvider,
) -> (
    u64,
    Option<(
        BTreeMap<OperationId, LedgerCharge>,
        BTreeMap<ChargeKey, u64>,
        BTreeMap<AdmissionClaimId, ReleasedClaim>,
    )>,
) {
    let ledger = provider.ledger.borrow();
    (ledger.next_token, normalized_ledger_image(&ledger))
}

#[test]
fn provider_reconciliation_funded_original_matching_mixed_and_released_images() {
    for rows in [0, 1, 259] {
        for (released, mixed) in [(false, false), (true, false), (false, true)] {
            let snapshot = image(rows, released, mixed);
            let provider = CapacityProvider::unavailable();
            provider.install_rebuilt_claims(&snapshot).unwrap();
            let before_ledger = ledger_image(&provider);
            let input = InputPermit::historical_test_input(1);
            let before = input.historical_test_remaining();
            let debit = bytes(snapshot.claims(), &provider.ledger.borrow()).unwrap();
            assert!(provider.validate_rebuilt_claims(&snapshot).is_ok());
            assert!(
                provider
                    .validate_rebuilt_claims_for_input(&snapshot, &input)
                    .is_ok(),
                "rows={rows} released={released} mixed={mixed} debit={debit}"
            );
            assert_eq!(STAGE_PROBE.with(|probe| probe.get().1), before - debit);
            assert_eq!(input.historical_test_remaining(), before);
            assert_eq!(ledger_image(&provider), before_ledger);
        }
    }
}

#[test]
fn provider_reconciliation_pressure_refuses_before_stage_and_preserves_live_ledger() {
    let snapshot = image(259, false, true);
    let provider = CapacityProvider::unavailable();
    provider.install_rebuilt_claims(&snapshot).unwrap();
    let before_ledger = ledger_image(&provider);
    let input = InputPermit::historical_test_input(1);
    let before = input.historical_test_remaining();
    let debit = bytes(snapshot.claims(), &provider.ledger.borrow()).unwrap();
    let held = input
        .reserve_historical_scratch(before - (debit - 1))
        .unwrap();
    STAGE_PROBE.with(|probe| probe.set((false, usize::MAX)));
    assert!(matches!(
        provider.validate_rebuilt_claims_for_input(&snapshot, &input),
        Err(CapacityProviderError::RebuildResourceBound)
    ));
    assert_eq!(STAGE_PROBE.with(|probe| probe.get().1), usize::MAX);
    assert_eq!(input.historical_test_remaining(), debit - 1);
    assert_eq!(ledger_image(&provider), before_ledger);
    drop(held);
    assert_eq!(input.historical_test_remaining(), before);
}

#[test]
fn provider_reconciliation_conflict_invalid_and_busy_preserve_original_results() {
    let snapshot = image(10, false, true);
    let provider = CapacityProvider::unavailable();
    provider.install_rebuilt_claims(&snapshot).unwrap();
    let changed = image(10, false, true);
    let before_ledger = ledger_image(&provider);
    let input = InputPermit::historical_test_input(1);
    let before = input.historical_test_remaining();
    assert!(matches!(
        provider.validate_rebuilt_claims(&changed),
        Err(CapacityProviderError::RebuildLedgerConflict)
    ));
    assert!(matches!(
        provider.validate_rebuilt_claims_for_input(&changed, &input),
        Err(CapacityProviderError::RebuildLedgerConflict)
    ));
    let invalid = OrdinaryClaimRebuildSnapshot::for_test(
        vec![RebuildClaim::for_test(
            OperationId::new(),
            AdmissionClaimId::new(),
            VolumeIdentity { device: 1 },
            FullCapacityAllocationClass::DestinationStaging,
            0,
            None,
            false,
        )],
        CapacityRebuildReadiness::ready(),
    );
    assert!(matches!(
        provider.validate_rebuilt_claims(&invalid),
        Err(CapacityProviderError::RebuildInvalidClaim)
    ));
    assert!(matches!(
        provider.validate_rebuilt_claims_for_input(&invalid, &input),
        Err(CapacityProviderError::RebuildInvalidClaim)
    ));
    let held = provider.ledger.borrow_mut();
    assert!(matches!(
        provider.validate_rebuilt_claims_for_input(&snapshot, &input),
        Err(CapacityProviderError::LedgerBusy)
    ));
    drop(held);
    assert_eq!(input.historical_test_remaining(), before);
    assert_eq!(ledger_image(&provider), before_ledger);
}

#[test]
fn provider_reconciliation_actual_stage_unwind_drops_graphs_before_exact_refund() {
    let snapshot = image(259, false, true);
    let provider = CapacityProvider::unavailable();
    provider.install_rebuilt_claims(&snapshot).unwrap();
    let before_ledger = ledger_image(&provider);
    let input = InputPermit::historical_test_input(1);
    let before = input.historical_test_remaining();
    let debit = bytes(snapshot.claims(), &provider.ledger.borrow()).unwrap();
    STAGE_PROBE.with(|probe| probe.set((true, 0)));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || provider.validate_rebuilt_claims_for_input(&snapshot, &input)
        ))
        .is_err()
    );
    assert_eq!(STAGE_PROBE.with(|probe| probe.get().1), before - debit);
    assert_eq!(input.historical_test_remaining(), before);
    assert_eq!(ledger_image(&provider), before_ledger);
}
