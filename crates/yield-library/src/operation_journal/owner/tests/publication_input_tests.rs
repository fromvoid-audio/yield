//! Original journal input accounting and actual runtime/store custody; no physical grant.
#[cfg(test)]
mod integration {
    use super::super::super::{
        CONTROL_INPUT_RESERVED_BYTES, InputBudget, InputClass, ORDINARY_INPUT_BYTES,
        PUBLICATION_INPUT_BYTES,
    };
    use super::super::*;
    use crate::operation_journal::store::{InsertTestBoundary, InsertTestDirective};
    fn shared() -> Arc<OwnerShared> {
        let (commands, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        shared
    }
    fn budget(shared: &Arc<OwnerShared>) -> InputBudget {
        *shared.input_bytes.lock().unwrap()
    }
    #[test]
    fn publication_input_exact_ceiling_retains_the_original_ordinary_and_control_margins() {
        let shared = shared();
        let publication = shared
            .reserve_publication_input(PUBLICATION_INPUT_BYTES)
            .unwrap();
        let saturated = budget(&shared);
        assert_eq!(saturated.publication_remaining, 0);
        assert_eq!(
            saturated.remaining[0],
            ORDINARY_INPUT_BYTES - PUBLICATION_INPUT_BYTES
        );
        assert_eq!(saturated.remaining[1], CONTROL_INPUT_RESERVED_BYTES);
        assert!(matches!(
            shared.reserve_publication_input(1),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: PUBLICATION_INPUT_BYTES,
                ..
            })
        ));
        assert_eq!(budget(&shared), saturated);
        let ordinary = shared
            .reserve_input(InputClass::Ordinary, saturated.remaining[0])
            .unwrap();
        let control = shared
            .reserve_input(InputClass::Control, CONTROL_INPUT_RESERVED_BYTES)
            .unwrap();
        assert_eq!(budget(&shared).remaining, [0, 0]);
        assert!(shared.reserve_input(InputClass::Ordinary, 1).is_err());
        assert!(shared.reserve_input(InputClass::Control, 1).is_err());
        drop(publication);
        assert_eq!(budget(&shared).remaining[0], PUBLICATION_INPUT_BYTES);
        assert_eq!(
            budget(&shared).publication_remaining,
            PUBLICATION_INPUT_BYTES
        );
        drop(control);
        drop(ordinary);
        assert_eq!(budget(&shared), InputBudget::new());
    }
    #[test]
    fn publication_input_ordinary_saturation_refuses_without_a_partial_publication_debit() {
        let shared = shared();
        let ordinary = shared
            .reserve_input(InputClass::Ordinary, ORDINARY_INPUT_BYTES)
            .unwrap();
        let before = budget(&shared);
        assert!(shared.reserve_publication_input(4096).is_err());
        assert_eq!(budget(&shared), before);
        assert!(shared.reserve_publication_input(usize::MAX).is_err());
        assert!(shared.reserve_publication_input(0).is_err());
        assert!(
            shared
                .reserve_input_kind(InputClass::Control, 1, true)
                .is_err()
        );
        assert_eq!(budget(&shared), before);
        drop(ordinary);
        let publication = shared.reserve_publication_input(4096).unwrap();
        drop(publication);
        assert_eq!(budget(&shared), InputBudget::new());
    }
    #[test]
    fn publication_input_actual_runtime_header_and_reader_staging_share_the_ceiling() {
        let f = fixture("publication-backing-runtime");
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        let shared = shared();
        runtime.attach_resource_custody(&shared).unwrap();
        assert!(shared.resource_admission.activate(|| true));
        let header = resource_custody::HEADER_BYTES;
        assert_eq!(
            budget(&shared).publication_remaining,
            PUBLICATION_INPUT_BYTES - header
        );
        let volume = VolumeIdentity { device: 123 };
        let initial = budget(&shared);
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(native_read_capsule::NativeReadError::Missing)
        ));
        assert_eq!(budget(&shared), initial);
        // Leave plentiful ordinary input but less than the original capsule's funded scratch.
        let retained = shared
            .reserve_publication_input(initial.publication_remaining - 65535)
            .unwrap();
        let full = budget(&shared);
        assert!(full.remaining[0] > 64 * 1024);
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(native_read_capsule::NativeReadError::Full)
        ));
        assert_eq!(budget(&shared), full);
        assert!(runtime.native_readers.is_empty());
        assert!(runtime.native_reader_registry_input.is_none());
        assert!(OperationJournalStore::open(&f.owner).is_err());
        drop(retained);
        assert_eq!(budget(&shared), initial);
        drop(runtime);
        assert_eq!(budget(&shared), InputBudget::new());
        assert!(OperationJournalStore::open(&f.owner).is_ok());
    }
    #[test]
    fn publication_input_composed_header_registry_and_reader_backing_refuse_the_thirty_second() {
        let shared = shared();
        // Original resource header, bounded mirror header, original reader registry.
        let headers = shared
            .reserve_publication_input(resource_custody::HEADER_BYTES + 4096 + 32 * 1024)
            .unwrap();
        let mut readers = Vec::new();
        for _ in 0..31 {
            readers.push(shared.reserve_publication_input(64 * 1024).unwrap());
        }
        let before = budget(&shared);
        assert!(matches!(
            shared.reserve_publication_input(64 * 1024),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: PUBLICATION_INPUT_BYTES,
                ..
            })
        ));
        assert_eq!(budget(&shared), before);
        drop(readers.pop());
        let replacement = shared.reserve_publication_input(64 * 1024).unwrap();
        assert_eq!(budget(&shared), before);
        drop(replacement);
        drop(readers);
        drop(headers);
        assert_eq!(budget(&shared), InputBudget::new());
        // This proves input accounting, not thirty-one qualified native volume capabilities.
    }
    #[test]
    fn publication_input_concurrent_original_permits_debit_and_refund_both_counters_once() {
        let shared = shared();
        let barrier = Arc::new(std::sync::Barrier::new(33));
        let workers: Vec<_> = (0..32)
            .map(|_| {
                let shared = Arc::clone(&shared);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let permit = shared.reserve_publication_input(64 * 1024).unwrap();
                    barrier.wait();
                    permit
                })
            })
            .collect();
        barrier.wait();
        let permits: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(budget(&shared).publication_remaining, 0);
        assert!(shared.reserve_publication_input(1).is_err());
        drop(permits);
        assert_eq!(budget(&shared), InputBudget::new());
    }
    #[test]
    fn publication_input_corrupt_subset_or_overflow_fences_the_original_owner_without_reset() {
        for corrupt in [0, 1, 2] {
            let shared = shared();
            {
                let mut b = shared.input_bytes.lock().unwrap();
                match corrupt {
                    0 => b.publication_remaining = usize::MAX,
                    1 => b.publication_remaining -= 1, // publication used exceeds all ordinary used
                    _ => b.remaining[0] = usize::MAX,
                }
            }
            let before = budget(&shared);
            assert!(matches!(
                shared.reserve_publication_input(1),
                Err(OperationJournalOwnerQueueError::Unavailable { .. })
            ));
            let after = budget(&shared);
            assert_eq!(after.remaining, before.remaining);
            assert_eq!(after.publication_remaining, before.publication_remaining);
            assert!(after.unavailable);
            assert!(matches!(
                shared.status(),
                OperationJournalOwnerStatus::Unavailable { .. }
            ));
        }
    }
    #[test]
    fn publication_input_unconfirmable_drop_and_poison_retain_unknown_credit_without_panicking() {
        for corrupt_tag in [false, true] {
            let shared = shared();
            let mut permit = shared.reserve_publication_input(4096).unwrap();
            if corrupt_tag {
                permit.publication = false;
            } else {
                permit.bytes = usize::MAX;
            }
            let before = budget(&shared);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(permit))).is_ok()
            );
            let after = budget(&shared);
            assert_eq!(after.remaining, before.remaining);
            assert_eq!(after.publication_remaining, before.publication_remaining);
            assert!(after.unavailable);
            assert!(shared.reserve_publication_input(1).is_err());
        }
        let shared = shared();
        let permit = shared.reserve_publication_input(4096).unwrap();
        let copy = Arc::clone(&shared);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _lock = copy.input_bytes.lock().unwrap();
                panic!("actual input accounting mutex poison");
            }))
            .is_err()
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(permit))).is_ok());
        assert!(matches!(
            shared.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        let poisoned = shared.input_bytes.lock().unwrap_err().into_inner();
        assert_eq!(
            poisoned.publication_remaining,
            PUBLICATION_INPUT_BYTES - 4096
        );
        assert_eq!(poisoned.remaining[0], ORDINARY_INPUT_BYTES - 4096);
    }
    #[test]
    fn publication_input_last_backing_alias_survives_actual_reply_loss_and_runtime_join() {
        let f = fixture("publication-backing-lost-reply");
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (ready, resume) = (Arc::clone(&entered), Arc::clone(&release));
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                ready.wait();
                resume.wait();
            }
            InsertTestDirective::Continue
        });
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let shared = Arc::clone(&owner.shared);
        let remaining = budget(&shared).publication_remaining;
        let backing = Arc::new(shared.reserve_publication_input(remaining).unwrap());
        let alias = Arc::clone(&backing);
        let id = OperationId::new();
        let (record, _) = v5_copy_record(id);
        let reply = owner.client().persist_intent(record).unwrap();
        entered.wait();
        drop(reply);
        drop(backing);
        assert_eq!(budget(&shared).publication_remaining, 0);
        release.wait();
        assert!(owner.shutdown());
        assert_eq!(
            budget(&shared).publication_remaining,
            PUBLICATION_INPUT_BYTES - remaining
        );
        assert_eq!(
            budget(&shared).remaining[0],
            ORDINARY_INPUT_BYTES - remaining
        );
        assert_eq!(budget(&shared).remaining[1], CONTROL_INPUT_RESERVED_BYTES);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert!(reopened.get(id).unwrap().is_some());
        drop(alias);
        assert_eq!(budget(&shared), InputBudget::new());
    }
}
