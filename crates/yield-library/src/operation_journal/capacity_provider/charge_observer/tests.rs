//! Actual original provider/lease arithmetic. No Native qualification or effect capability.
use super::super::tests::{facts, plan_and_claims, rebuild_claim, rebuild_snapshot};
use super::*;
use std::sync::{Barrier, mpsc};
const CLASS: FullCapacityAllocationClass = FullCapacityAllocationClass::DestinationStaging;
fn provider(devices: &[u64]) -> CapacityProvider {
    CapacityProvider::qualified_fixture(
        devices
            .iter()
            .map(|device| {
                (
                    VolumeIdentity { device: *device },
                    facts(*device, PROTECTED_FREE_SPACE_FLOOR + 1_000_000, 4096),
                )
            })
            .collect(),
    )
}
fn admit(
    provider: &CapacityProvider,
    device: u64,
    class: FullCapacityAllocationClass,
) -> ProvisionalCapacityLease {
    let (plan, claims) = plan_and_claims(&[(VolumeIdentity { device }, class, 4096, 4096)]);
    provider.admit(&plan, &claims).unwrap()
}
#[test]
fn actual_admission_drop_pin_and_confirmed_rollback_advance_current_totals() {
    let provider = provider(&[1, 2]);
    assert!(
        provider
            .publication_charge_observer(&VolumeIdentity { device: 1 })
            .is_err()
    );
    let a = admit(&provider, 1, CLASS);
    let observer = provider
        .publication_charge_observer(&VolumeIdentity { device: 1 })
        .unwrap();
    let copy = provider
        .publication_charge_observer(&VolumeIdentity { device: 1 })
        .unwrap();
    assert!(Arc::ptr_eq(&observer, &copy));
    let initial = observer.observation().unwrap();
    assert_eq!(initial.incremental_bytes, 4096);
    let existing = admit(
        &provider,
        1,
        FullCapacityAllocationClass::ExistingDestination,
    );
    let recovery = admit(
        &provider,
        1,
        FullCapacityAllocationClass::ExistingRecoveryPayload,
    );
    let other = admit(&provider, 2, CLASS);
    assert_eq!(observer.observation().unwrap().incremental_bytes, 4096);
    let mut second = admit(&provider, 1, CLASS);
    assert_eq!(observer.observation().unwrap().incremental_bytes, 8192);
    assert!(observer.with_current(Some(initial), |_| ()).is_none());
    let operation = OperationId::new();
    let before_pin = observer.observation().unwrap();
    second.pin_for_persistence(operation).unwrap();
    assert!(observer.observation().unwrap().generation > before_pin.generation);
    let before_refusal = observer.observation();
    assert_eq!(second.release(), Err(CapacityProviderError::LeaseRetained));
    assert_eq!(observer.observation(), before_refusal);
    assert_eq!(observer.observation().unwrap().incremental_bytes, 8192);
    second.release_after_confirmed_rollback(operation).unwrap();
    assert_eq!(observer.observation().unwrap().incremental_bytes, 4096);
    drop(a);
    assert_eq!(observer.observation().unwrap().incremental_bytes, 0);
    drop((existing, recovery, other));
    provider.prove_no_live_charges().unwrap();
    drop(provider);
    assert!(observer.observation().is_none());
}
#[test]
fn actual_rebuild_durable_release_and_replay_keep_exact_original_totals() {
    let provider = CapacityProvider::unavailable();
    let volume = VolumeIdentity { device: 7 };
    let operation = OperationId::new();
    let claim = AdmissionClaimId::new();
    let image = rebuild_snapshot(vec![rebuild_claim(
        operation,
        claim,
        volume.clone(),
        CLASS,
        8192,
        None,
        false,
    )]);
    provider.install_rebuilt_claims(&image).unwrap();
    let observer = provider.publication_charge_observer(&volume).unwrap();
    let first = observer.observation().unwrap();
    assert_eq!(first.incremental_bytes, 8192);
    provider.install_rebuilt_claims(&image).unwrap();
    assert_eq!(observer.observation(), Some(first));
    let receipt = OrdinaryClaimReleaseReceipt::for_test(
        operation,
        claim,
        Uuid::new_v4(),
        volume,
        CLASS,
        8192,
    );
    provider.release_claim(&receipt).unwrap();
    let released = observer.observation().unwrap();
    assert!(released.generation > first.generation);
    assert_eq!(released.incremental_bytes, 0);
    provider.release_claim(&receipt).unwrap();
    assert_eq!(observer.observation(), Some(released));
    assert!(provider.install_rebuilt_claims(&image).is_err());
    assert_eq!(observer.observation(), Some(released));
}
#[test]
fn actual_owner_admission_is_pending_before_write_and_invalidates_reader_generation() {
    std::thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let (ack, command) = mpsc::channel();
        let actor_barrier = barrier.clone();
        let actor = scope.spawn(move || {
            let provider = provider(&[1]);
            let first = admit(&provider, 1, CLASS);
            let observer = provider
                .publication_charge_observer(&VolumeIdentity { device: 1 })
                .unwrap();
            tx.send(observer).unwrap();
            command.recv().unwrap();
            *provider.mirror.1.lock().unwrap() = Some(actor_barrier);
            let second = admit(&provider, 1, CLASS);
            tx.send(
                provider
                    .publication_charge_observer(&VolumeIdentity { device: 1 })
                    .unwrap(),
            )
            .unwrap();
            command.recv().unwrap();
            drop((first, second));
        });
        let observer = rx.recv().unwrap();
        let captured_before_native_read = observer.observation().unwrap();
        ack.send(()).unwrap();
        barrier.wait();
        assert!(observer.observation().is_none());
        assert!(
            observer
                .with_current(Some(captured_before_native_read), |_| ())
                .is_none()
        );
        barrier.wait();
        let same = rx.recv().unwrap();
        assert!(Arc::ptr_eq(&same, &observer));
        assert_eq!(observer.observation().unwrap().incremental_bytes, 8192);
        assert!(
            observer
                .with_current(Some(captured_before_native_read), |_| ())
                .is_none()
        );
        ack.send(()).unwrap();
        actor.join().unwrap();
        assert!(observer.observation().is_none());
    });
}
#[test]
fn thirty_two_actual_bound_volumes_reuse_only_expired_observers() {
    let provider = provider(&(0..33).collect::<Vec<_>>());
    let leases: Vec<_> = (0..33)
        .map(|device| admit(&provider, device, CLASS))
        .collect();
    let mut observers: Vec<_> = (0..32)
        .map(|device| {
            provider
                .publication_charge_observer(&VolumeIdentity { device })
                .unwrap()
        })
        .collect();
    assert!(
        provider
            .publication_charge_observer(&VolumeIdentity { device: 32 })
            .is_err()
    );
    let retained = observers[0].clone();
    drop(observers.remove(0));
    assert!(
        provider
            .publication_charge_observer(&VolumeIdentity { device: 32 })
            .is_err()
    );
    drop(retained);
    let last = provider
        .publication_charge_observer(&VolumeIdentity { device: 32 })
        .unwrap();
    assert_eq!(last.observation().unwrap().incremental_bytes, 4096);
    drop(leases);
    assert_eq!(last.observation().unwrap().incremental_bytes, 0);
}
#[test]
fn original_mutation_unwind_overflow_and_poison_never_reopen_observations() {
    for failure in 0..3 {
        let provider = provider(&[1]);
        let lease = admit(&provider, 1, CLASS);
        let observer = provider
            .publication_charge_observer(&VolumeIdentity { device: 1 })
            .unwrap();
        match failure {
            0 => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let ledger = provider.ledger.borrow_mut();
                    let _pending = ChargeMutation::begin(&ledger);
                    panic!("unknown original accounting completion");
                }));
                assert!(result.is_err());
            }
            1 => provider.mirror.0.lock().unwrap().generation = u64::MAX,
            _ => {
                let mirror = provider.mirror.clone();
                assert!(
                    std::thread::spawn(move || {
                        let _guard = mirror.0.lock().unwrap();
                        panic!("poison scalar observation");
                    })
                    .join()
                    .is_err()
                );
            }
        }
        drop(lease);
        assert!(observer.observation().is_none());
        let another = admit(&provider, 1, CLASS);
        assert!(observer.observation().is_none());
        assert!(
            provider
                .publication_charge_observer(&VolumeIdentity { device: 1 })
                .is_err()
        );
        drop(another);
        provider.prove_no_live_charges().unwrap();
    }
}

#[test]
fn checked_incremental_total_overflow_sticks_closed_without_refunding_original_charge() {
    let provider = provider(&[1]);
    let lease = admit(&provider, 1, CLASS);
    let observer = provider
        .publication_charge_observer(&VolumeIdentity { device: 1 })
        .unwrap();
    {
        let mut ledger = provider.ledger.borrow_mut();
        let pending = ChargeMutation::begin(&ledger);
        // Deliberately unknown actual-ledger image, not a supplied native fact.
        ledger.aggregate_charges.insert(
            ChargeKey {
                volume: VolumeIdentity { device: 1 },
                allocation_class: FullCapacityAllocationClass::JournalRecord,
            },
            u64::MAX,
        );
        pending.finish(&ledger);
    }
    assert!(observer.observation().is_none());
    drop(lease);
    assert!(observer.observation().is_none());
    assert!(provider.prove_no_live_charges().is_err());
}
