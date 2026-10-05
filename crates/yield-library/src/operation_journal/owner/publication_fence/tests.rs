//! Production scalar fences only; no source pin or native effect authority is constructed.
use super::super::OwnerCommandReceivers;
use super::super::resource_custody::ResourceAdmission;
use super::*;
fn shared() -> Arc<OwnerShared> {
    let (commands, _receivers) = OwnerCommandReceivers::channel();
    Arc::new(OwnerShared::new(commands))
}
#[test]
fn original_fence_registry_bounds_live_custody_and_reclaims_only_unentered_last_aliases() {
    let shared = shared();
    let mut registry = Registry::new();
    let mut held: Vec<_> = (0..32)
        .map(|_| registry.issue(OperationId::new(), &shared).unwrap())
        .collect();
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
    let operation = held[0].fence.operation;
    assert!(held[0].enter());
    drop(held.remove(0));
    assert!(matches!(
        registry.close_for_cancellation(operation),
        Err(OperationJournalOwnerError::PublicationCancellationPending { .. })
    ));
    drop(held);
    let next: Vec<_> = (0..31)
        .map(|_| registry.issue(OperationId::new(), &shared).unwrap())
        .collect();
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    assert!(
        registry
            .slots
            .iter()
            .flatten()
            .any(|f| f.operation == operation)
    );
    drop(next);
    drop(registry);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES
    );
}
#[test]
fn original_fence_cancellation_and_admission_entry_have_one_winner() {
    for _ in 0..64 {
        let shared = shared();
        let mut registry = Registry::new();
        let operation = OperationId::new();
        let mut pending = registry.issue(operation, &shared).unwrap();
        let admission = ResourceAdmission::new();
        assert!(admission.activate(|| true));
        let mut runtime = admission.begin_read(|| true).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let entry = scope.spawn(|| {
                barrier.wait();
                runtime
                    .enter_effect_checked(|| pending.enter_checked())
                    .is_ok()
            });
            barrier.wait();
            let cancelled = registry.close_for_cancellation(operation).is_ok();
            assert_ne!(entry.join().unwrap(), cancelled);
        });
        drop(pending);
        drop(runtime);
        assert_eq!(admission.snapshot().1, 0);
    }
}
#[test]
fn original_fence_exhaustion_and_poison_never_remint_or_cancel_uncertain_entry() {
    let shared = shared();
    let mut registry = Registry::new();
    registry.next = u64::MAX;
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    registry.next = 0;
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    let mut registry = Registry::new();
    let operation = OperationId::new();
    let mut pending = registry.issue(operation, &shared).unwrap();
    let fence = Arc::clone(&pending.fence);
    assert!(
        std::panic::catch_unwind(|| {
            let _locked = fence.state.lock().unwrap();
            panic!("original fence mutex poison");
        })
        .is_err()
    );
    assert!(!pending.enter());
    assert!(registry.close_for_cancellation(operation).is_err());
    drop(pending);
    assert!(registry.issue(operation, &shared).is_err());
}

#[test]
fn original_fence_unknown_entry_quarantines_original_admission_without_reverse_locking() {
    let shared = shared();
    let admission = &shared.resource_admission;
    assert!(admission.activate(|| true));
    let mut registry = Registry::new();
    let operation = OperationId::new();
    let mut pending = registry.issue(operation, &shared).unwrap();
    let fence = pending.fence.clone();
    assert!(
        std::panic::catch_unwind(|| {
            let _lock = fence.state.lock().unwrap();
            panic!("poison original fence");
        })
        .is_err()
    );
    let mut runtime = admission.begin_read(|| true).unwrap();
    assert_eq!(
        runtime.enter_effect_checked(|| pending.enter_checked()),
        Err(UseError::Unknown)
    );
    assert_eq!(admission.validate_open(), Err(UseError::Unknown));
    assert_eq!(admission.snapshot().1, 1);
    drop(pending);
    drop(runtime);
    assert_eq!(admission.snapshot().1, 0);
    assert!(!admission.close_and_wait());
    assert!(registry.issue(OperationId::new(), &shared).is_err());
}
