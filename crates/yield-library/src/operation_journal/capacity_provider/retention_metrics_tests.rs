use super::tests::{facts, plan_and_claims};
use super::*;
use crate::operation_journal::capacity_retention_telemetry::{
    CapacityLedgerCall as K, CapacityLedgerReturn as R, tests::Capture,
};

fn fixture() -> (
    CapacityProvider,
    ProvisionalCapacityLease,
    AdmissionClaimInventory,
    VolumeIdentity,
) {
    let volume = VolumeIdentity { device: 802 };
    let (plan, claims) = plan_and_claims(&[(
        volume.clone(),
        FullCapacityAllocationClass::SourceDatabaseCommit,
        4096,
        4096,
    )]);
    let provider = CapacityProvider::qualified_fixture(BTreeMap::from([(
        volume.clone(),
        facts(802, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
    )]));
    let lease = provider.admit(&plan, &claims).unwrap();
    (provider, lease, claims, volume)
}
#[test]
fn actual_provisional_rollback_keeps_both_original_primitive_returns() {
    let capture = Capture::new();
    let (provider, mut lease, _, volume) = fixture();
    let token = provider.next_token();
    lease
        .release_after_confirmed_rollback(OperationId::new())
        .unwrap();
    assert_eq!(provider.next_token(), token);
    assert_eq!(provider.claim_count(), 0);
    assert_eq!(
        provider.outstanding(&volume, FullCapacityAllocationClass::SourceDatabaseCommit),
        0
    );
    let snapshot = capture.snapshot();
    assert_eq!(snapshot.count(K::ConfirmedRollbackRelease, R::Confirmed), 1);
    assert_eq!(snapshot.count(K::ProvisionalRelease, R::Confirmed), 1);
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 2);
    drop(lease);
    assert_eq!(capture.snapshot(), snapshot);
}

#[test]
fn actual_release_failure_busy_confirmation_and_drop_preserve_original_tokens() {
    let capture = Capture::new();
    let (provider, mut lease, _, volume) = fixture();
    let token = provider.next_token();
    provider.ledger.borrow_mut().fail_release = true;
    assert_eq!(lease.release(), Err(CapacityProviderError::ReleaseFailed));
    provider.ledger.borrow_mut().fail_release = false;
    {
        let _held = provider.ledger.borrow_mut();
        assert_eq!(lease.release(), Err(CapacityProviderError::LedgerBusy));
    }
    assert_eq!(provider.claim_count(), 1);
    assert_eq!(
        provider.outstanding(&volume, FullCapacityAllocationClass::SourceDatabaseCommit),
        4096
    );
    assert_eq!(provider.next_token(), token);
    lease.release().unwrap();
    lease.release().unwrap();
    let before_drop = capture.snapshot();
    drop(lease);
    assert_eq!(capture.snapshot(), before_drop);
    assert_eq!(before_drop.count(K::ProvisionalRelease, R::Refused), 1);
    assert_eq!(before_drop.count(K::ProvisionalRelease, R::Busy), 1);
    assert_eq!(before_drop.count(K::ProvisionalRelease, R::Confirmed), 2);
    assert_eq!(provider.claim_count(), 0);
    let (_, lease, _, _) = fixture();
    drop(lease);
    assert_eq!(
        capture
            .snapshot()
            .count(K::ProvisionalRelease, R::Confirmed),
        3
    );
}
#[test]
fn actual_pin_durable_replay_retention_and_exact_rollback_are_only_observations() {
    let capture = Capture::new();
    let (provider, mut lease, _, volume) = fixture();
    let operation = OperationId::new();
    lease.pin_for_persistence(operation).unwrap();
    lease.pin_for_persistence(operation).unwrap();
    assert_eq!(lease.release(), Err(CapacityProviderError::LeaseRetained));
    assert_eq!(
        lease.release_after_confirmed_rollback(OperationId::new()),
        Err(CapacityProviderError::LeaseDispositionConflict)
    );
    assert_eq!(provider.claim_count(), 1);
    lease.release_after_confirmed_rollback(operation).unwrap();
    assert_eq!(provider.claim_count(), 0);
    assert_eq!(
        provider.outstanding(&volume, FullCapacityAllocationClass::SourceDatabaseCommit),
        0
    );
    let (provider, mut durable, _, _) = fixture();
    durable.pin_for_persistence(operation).unwrap();
    provider.ledger.borrow_mut().fail_disposition = true;
    assert_eq!(
        durable.mark_durable(operation),
        Err(CapacityProviderError::LedgerBusy)
    );
    assert!(matches!(
        durable.disposition(),
        ProvisionalLeaseDisposition::AuditPending { .. }
    ));
    provider.ledger.borrow_mut().fail_disposition = false;
    durable.mark_durable(operation).unwrap();
    durable.mark_durable(operation).unwrap();
    assert_eq!(durable.release(), Err(CapacityProviderError::LeaseRetained));
    let before_drop = capture.snapshot();
    drop(durable);
    assert_eq!(capture.snapshot(), before_drop);
    assert_eq!(provider.claim_count(), 1);
    assert_eq!(before_drop.count(K::AuditPendingBinding, R::Confirmed), 3);
    assert_eq!(before_drop.count(K::DurableBinding, R::Confirmed), 2);
    assert_eq!(before_drop.count(K::DurableBinding, R::Busy), 1);
    assert_eq!(before_drop.count(K::ProvisionalRelease, R::Retained), 2);
    assert_eq!(
        before_drop.count(K::ConfirmedRollbackRelease, R::Refused),
        1
    );
    assert_eq!(
        before_drop.count(K::ConfirmedRollbackRelease, R::Confirmed),
        1
    );
}
#[test]
fn actual_ordinary_release_busy_retry_and_confirmation_preserve_original_claim() {
    let capture = Capture::new();
    let (provider, mut lease, claims, volume) = fixture();
    let operation = OperationId::new();
    lease.mark_durable(operation).unwrap();
    let claim = &claims.claims()[0];
    let receipt = OrdinaryClaimReleaseReceipt::for_test(
        operation,
        claim.claim_id(),
        Uuid::new_v4(),
        volume.clone(),
        claim.allocation_class(),
        claim.allocated_bytes(),
    );
    {
        let _held = provider.ledger.borrow_mut();
        assert_eq!(
            provider.release_claim(&receipt),
            Err(CapacityProviderError::LedgerBusy)
        );
    }
    assert_eq!(provider.claim_count(), 1);
    assert_eq!(
        provider.outstanding(&volume, FullCapacityAllocationClass::SourceDatabaseCommit),
        4096
    );
    provider.release_claim(&receipt).unwrap();
    provider.release_claim(&receipt).unwrap();
    assert_eq!(provider.claim_count(), 0);
    assert_eq!(
        capture.snapshot().count(K::OrdinaryClaimRelease, R::Busy),
        1
    );
    assert_eq!(
        capture
            .snapshot()
            .count(K::OrdinaryClaimRelease, R::Confirmed),
        2
    );
}
