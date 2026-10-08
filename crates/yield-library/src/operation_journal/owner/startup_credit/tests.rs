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
    assert_eq!(shared.resource_admission.snapshot().0, true);
    assert_eq!(
        shared.resource_admission.validate_open(),
        Err(resource_custody::UseError::Unknown)
    );
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
    assert_eq!(shared.resource_admission.snapshot().0, true);
    assert_eq!(
        shared.resource_admission.validate_open(),
        Err(resource_custody::UseError::Unknown)
    );
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
    assert_eq!(shared.resource_admission.snapshot().0, true);
    assert_eq!(
        shared.resource_admission.validate_open(),
        Err(resource_custody::UseError::Unknown)
    );
}

#[test]
fn original_publication_scratch_preserves_both_debits_through_detached_keeper() {
    let (commands, _receivers) = OwnerCommandReceivers::channel();
    let shared = Arc::new(OwnerShared::new(commands));
    let before = {
        let budget = shared.input_bytes.lock().unwrap();
        (budget.remaining[0], budget.publication_remaining)
    };
    let root = shared.reserve_publication_input(64 * 1024).unwrap();
    let scratch = root
        .reserve_historical_scratch(4096)
        .expect("original publication producer scratch");
    let keeper = scratch.into_startup_credit();
    drop(root);
    let after_root = {
        let budget = shared.input_bytes.lock().unwrap();
        (budget.remaining[0], budget.publication_remaining)
    };
    assert_eq!(after_root, (before.0 - 4096, before.1 - 4096));
    drop(keeper);
    let after = {
        let budget = shared.input_bytes.lock().unwrap();
        (budget.remaining[0], budget.publication_remaining)
    };
    assert_eq!(after, before);
    assert!(!matches!(
        shared.status(),
        OperationJournalOwnerStatus::Unavailable { .. }
    ));
}

#[test]
fn detached_publication_keeper_releases_original_owner_before_final_pool_refund() {
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let weak = Arc::downgrade(&shared);
    let pool = shared.input_bytes.clone();
    let credit = shared
        .reserve_publication_input(8192)
        .unwrap()
        .into_startup_credit();
    drop(shared);
    assert!(weak.upgrade().is_none());
    drop(weak);
    assert_eq!(
        pool.lock().unwrap().publication_remaining,
        baseline.publication_remaining - 8192
    );
    drop(credit);
    assert_eq!(*pool.lock().unwrap(), baseline);
}
