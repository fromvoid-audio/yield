//! Exercise the production composition against the original scalar admission.
//! Callback sentinels verify custody/ordering only; they confer no source/native authority.
use super::super::resource_custody::UseError;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn original_grant_custody_closed_unknown_and_full_refuse_before_source_acquisition() {
    for unknown in [false, true] {
        let original = ResourceAdmission::new();
        assert!(original.activate(|| true));
        let issued = original.clone();
        if unknown {
            original.invalidate();
        } else {
            original.close();
        }
        assert!(
            counted_entry(&issued, || -> Result<(), OperationJournalOwnerError> {
                panic!("source acquisition after original admission closure")
            })
            .is_err()
        );
        assert_eq!(original.snapshot().1, 0);
        assert_eq!(
            original.validate_open(),
            Err(if unknown {
                UseError::Unknown
            } else {
                UseError::Closed
            })
        );
        assert_eq!(original.close_and_wait(), !unknown);
    }
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    let retained: Vec<_> = (0..32)
        .map(|_| original.begin_read(|| true).unwrap())
        .collect();
    assert!(
        counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
            panic!("source acquisition when original use ceiling is full")
        })
        .is_err()
    );
    assert_eq!(original.snapshot().1, 32);
    drop(retained);
    assert!(original.close_and_wait());
}

#[test]
fn original_grant_custody_source_failure_and_close_during_acquisition_release_without_entry() {
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    assert!(
        counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
            assert_eq!(original.snapshot().1, 1);
            Err(OperationJournalOwnerError::StagingGrant {
                reason: "source acquisition refused".into(),
            })
        })
        .is_err()
    );
    assert_eq!(original.snapshot(), (false, 0, false));
    struct SourceSentinel(Arc<ResourceAdmission>, Arc<AtomicBool>);
    impl Drop for SourceSentinel {
        fn drop(&mut self) {
            assert_eq!(
                self.0.snapshot().1,
                1,
                "source releases while runtime remains retained"
            );
            self.1.store(true, Ordering::Release);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    assert!(
        counted_entry(&original, || {
            original.close();
            Ok(SourceSentinel(original.clone(), dropped.clone()))
        })
        .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(original.snapshot(), (true, 0, false));
    assert!(original.close_and_wait());
}

#[test]
fn original_grant_custody_entry_preserves_count_and_teardown_waits_without_settlement() {
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    struct SourceSentinel(Arc<ResourceAdmission>);
    impl Drop for SourceSentinel {
        fn drop(&mut self) {
            assert_eq!(
                self.0.snapshot().1,
                1,
                "source drops before original runtime use"
            );
        }
    }
    let entered = counted_entry(&original, || {
        assert_eq!(original.snapshot().1, 1);
        Ok(SourceSentinel(original.clone()))
    })
    .unwrap();
    assert_eq!(original.snapshot(), (false, 1, false));
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let closer = scope.spawn(|| {
            original.close();
            tx.send(()).unwrap();
            assert!(
                !original.close_and_wait(),
                "entered Drop never settles or makes clean handoff"
            );
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
                panic!("new source entry after closing")
            })
            .is_err()
        );
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(entered);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        closer.join().unwrap();
    });
    assert_eq!(original.snapshot(), (true, 0, true));
    assert!(!original.activate(|| true));
}

#[test]
fn original_target_expiring_during_source_acquisition_refuses_count_upgrade() {
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    struct Custody(Arc<ResourceAdmission>, Arc<AtomicBool>);
    impl Drop for Custody {
        fn drop(&mut self) {
            assert_eq!(self.0.snapshot().1, 1);
            self.1.store(true, Ordering::Release);
        }
    }
    let released = Arc::new(AtomicBool::new(false));
    assert!(
        counted_entry_checked(
            &original,
            || Ok(Custody(original.clone(), released.clone())),
            |_| Err(UseError::Closed)
        )
        .is_err()
    );
    assert!(released.load(Ordering::Acquire));
    assert_eq!(original.snapshot(), (false, 0, false));
    assert!(original.close_and_wait());
}

#[test]
fn counted_entry_retains_original_publication_credit_until_source_and_target_drop() {
    let (commands, _receivers) = super::super::OwnerCommandReceivers::channel();
    let shared = Arc::new(super::super::OwnerShared::new(commands));
    let admission = &shared.resource_admission;
    assert!(admission.activate(|| true));
    let input = shared.reserve_publication_input(64 * 1024).unwrap();
    let initial = super::super::PUBLICATION_INPUT_BYTES;
    struct Custody(super::super::InputPermit);
    let entered = counted_entry_checked(admission, || Ok(Custody(input)), |_| Ok(())).unwrap();
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        initial - 64 * 1024
    );
    assert_eq!(entered._source.0.bytes, 64 * 1024);
    assert_eq!(admission.snapshot().1, 1);
    drop(entered);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        initial
    );
    assert_eq!(admission.snapshot(), (false, 0, true));
}

#[test]
fn publication_charge_generation_change_during_counted_source_acquisition_refuses_final_entry() {
    use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity};
    use crate::operation_journal::admission_claims::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory,
    };
    use crate::operation_journal::capacity_provider::CapacityProvider;
    use crate::operation_journal::full_capacity::{
        FullCapacityAllocation, FullCapacityAllocationClass, FullCapacityPlan,
    };
    use crate::operation_journal::owner::{OperationId, OwnerCommandReceivers, OwnerShared};
    for changed in [false, true] {
        let volume = VolumeIdentity { device: 1 };
        let plan = FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                volume.clone(),
                1,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                1,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap(),
        ])
        .unwrap();
        let claims = AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                volume.clone(),
                FullCapacityAllocationClass::DestinationStaging,
                1,
            )
            .unwrap(),
        ])
        .unwrap();
        let provider = CapacityProvider::qualified_fixture(
            [(
                volume.clone(),
                VolumeFacts {
                    identity: volume.clone(),
                    allocation_unit: 1,
                    free_bytes: PROTECTED_FREE_SPACE_FLOOR + 1024,
                },
            )]
            .into(),
        );
        let operation = OperationId::new();
        let mut lease = provider.admit(&plan, &claims).unwrap();
        lease.mark_durable(operation).unwrap();
        let (commands, _) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        let mut registry = super::super::publication_fence::Registry::new();
        let mut fence = registry.issue(operation, &shared).unwrap();
        let binding = provider
            .bind_publication_charge(operation, &claims.claims()[0])
            .unwrap();
        let admission = ResourceAdmission::new();
        assert!(admission.activate(|| true));
        struct SourceSentinel(Arc<ResourceAdmission>, Arc<AtomicBool>);
        impl Drop for SourceSentinel {
            fn drop(&mut self) {
                assert_eq!(self.0.snapshot().1, 1);
                self.1.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let result = counted_entry_checked(
            &admission,
            || {
                assert_eq!(admission.snapshot().1, 1);
                if changed {
                    let extra = AdmissionClaimInventory::new(vec![
                        AdmissionClaim::new(
                            AdmissionClaimId::new(),
                            volume.clone(),
                            FullCapacityAllocationClass::DestinationStaging,
                            1,
                        )
                        .unwrap(),
                    ])
                    .unwrap();
                    // The actual provider mutation changes generation; dropping the provisional
                    // lease returns only its own bytes and cannot revive the old observation.
                    drop(provider.admit(&plan, &extra).unwrap());
                }
                Ok(SourceSentinel(admission.clone(), dropped.clone()))
            },
            |_| enter_charge_fence(&binding, &mut fence),
        );
        if changed {
            assert!(result.is_err());
            assert!(!binding.is_current());
        } else {
            assert!(result.is_ok());
            assert!(binding.is_current());
        }
        drop(result);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(admission.snapshot().1, 0);
        if changed {
            registry.close_for_cancellation(operation).unwrap();
            assert!(!fence.enter());
        }
    }
}
