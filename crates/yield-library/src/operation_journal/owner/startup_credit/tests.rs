use super::*;

fn shared() -> Arc<OwnerShared> {
    let (commands, _receivers) = OwnerCommandReceivers::channel();
    let shared = Arc::new(OwnerShared::new(commands));
    shared
        .lifecycle
        .store(OwnerLifecycle::Available as u8, Ordering::Release);
    assert!(shared.resource_admission.activate(|| true));
    shared
}

#[test]
fn detached_original_pool_refunds_exactly_without_retaining_shared() {
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let weak = Arc::downgrade(&shared);
    let pool = shared.input_bytes.clone();
    let credit = shared
        .reserve_input(InputClass::Ordinary, 8192)
        .unwrap()
        .into_startup_credit();
    assert_eq!(
        pool.lock().unwrap().remaining[0],
        baseline.remaining[0] - 8192
    );
    drop(shared);
    assert!(weak.upgrade().is_none());
    drop(weak); // test-only weak does not pin a funded component header
    drop(credit);
    assert_eq!(*pool.lock().unwrap(), baseline);
}

#[test]
fn detached_original_refund_corruption_closes_actual_lifecycle_and_resource_gate() {
    let shared = shared();
    let credit = shared
        .reserve_input(InputClass::Ordinary, 8192)
        .unwrap()
        .into_startup_credit();
    shared.input_bytes.lock().unwrap().remaining[0] = InputBudget::shared_initial().remaining[0];
    drop(credit);
    assert!(shared.input_bytes.lock().unwrap().unavailable);
    assert_eq!(
        shared.lifecycle.load(Ordering::Acquire),
        OwnerLifecycle::Unavailable as u8
    );
    assert_eq!(shared.resource_admission.snapshot().0, false);
    assert!(matches!(
        shared.status(),
        OperationJournalOwnerStatus::Unavailable { .. }
    ));
}

#[test]
fn detached_original_refund_poison_closes_actual_lifecycle_and_resource_gate() {
    let shared = shared();
    let credit = shared
        .reserve_input(InputClass::Ordinary, 8192)
        .unwrap()
        .into_startup_credit();
    let poisoned = shared.clone();
    let worker = std::thread::spawn(move || {
        let _guard = poisoned.input_bytes.lock().unwrap();
        panic!("original input pool poison fixture");
    });
    assert!(worker.join().is_err());
    drop(credit);
    assert_eq!(
        shared.lifecycle.load(Ordering::Acquire),
        OwnerLifecycle::Unavailable as u8
    );
    assert_eq!(shared.resource_admission.snapshot().0, false);
}

#[test]
fn permanent_startup_debit_cannot_mask_corrupt_publication_accounting() {
    let shared = shared();
    let baseline = InputBudget::shared_initial();
    shared.input_bytes.lock().unwrap().publication_remaining -= 1;
    assert!(matches!(
        shared.reserve_input(InputClass::Control, 1),
        Err(OperationJournalOwnerQueueError::Unavailable { .. })
    ));
    let budget = *shared.input_bytes.lock().unwrap();
    assert!(budget.unavailable);
    assert_eq!(budget.remaining, baseline.remaining);
    assert_eq!(
        budget.publication_remaining,
        baseline.publication_remaining - 1
    );
    assert_eq!(
        shared.lifecycle.load(Ordering::Acquire),
        OwnerLifecycle::Unavailable as u8
    );
    assert_eq!(shared.resource_admission.snapshot().0, false);
}
