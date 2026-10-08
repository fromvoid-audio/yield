//! Production scalar fences only; no source pin or native effect authority is constructed.
use super::super::resource_custody::ResourceAdmission;
use super::super::OwnerCommandReceivers;
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
    assert!(registry
        .slots
        .iter()
        .flatten()
        .any(|f| f.operation == operation));
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
    let fence = pending.fence.clone();
    assert!(std::panic::catch_unwind(|| {
        let _locked = fence.state.lock().unwrap();
        panic!("original fence mutex poison");
    })
    .is_err());
    assert!(!pending.enter());
    assert!(registry.close_for_cancellation(operation).is_err());
    assert!(matches!(registry.close_for_mutation(operation),
        Err(OperationJournalOwnerError::PublicationMutationPending { operation_id }) if operation_id == operation));

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
    assert!(std::panic::catch_unwind(|| {
        let _lock = fence.state.lock().unwrap();
        panic!("poison original fence");
    })
    .is_err());
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

#[test]
fn publication_source_unknown_fence_refuses_only_its_original_registration() {
    let shared = shared();
    let mut registry = Registry::new();
    let source = SourceRegistrationId::model_key_for_tests(41);
    let operation = OperationId::new();
    let pending = registry
        .issue_for_source(operation, source, &shared)
        .unwrap();
    *pending.fence.state.lock().unwrap() = State::Unknown;
    assert!(matches!(registry.close_for_source(source),
        Err(OperationJournalOwnerError::PublicationSourceRetirementPending { operation_id }) if operation_id == operation));
    assert!(registry
        .close_for_source(SourceRegistrationId::model_key_for_tests(42))
        .is_ok());
    assert_eq!(*pending.fence.state.lock().unwrap(), State::Unknown);
}

#[test]
fn publication_source_retirement_and_original_admission_entry_have_one_winner() {
    for _ in 0..64 {
        let shared = shared();
        let mut registry = Registry::new();
        let operation = OperationId::new();
        let source = SourceRegistrationId::model_key_for_tests(37);
        let mut pending = registry
            .issue_for_source(operation, source, &shared)
            .unwrap();
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
            let cancelled = registry.close_for_source(source).is_ok();
            assert_ne!(entry.join().unwrap(), cancelled);
        });
        drop(pending);
        drop(runtime);
        assert_eq!(admission.snapshot().1, 0);
    }
}

#[test]
fn original_completion_coalesces_without_reclaiming_or_settling() {
    let shared = shared();
    let mut registry = Registry::new();
    let operation = OperationId::new();
    let pending = registry.issue(operation, &shared).unwrap();
    pending.complete_before_effect([7; 32], true);
    assert_eq!(
        *pending.fence.state.lock().unwrap(),
        State::CompletedNoEffect {
            digest: [7; 32],
            observed: false
        }
    );
    registry.poll_completions();
    pending.complete_before_effect([7; 32], true);
    assert_eq!(
        *pending.fence.state.lock().unwrap(),
        State::CompletedNoEffect {
            digest: [7; 32],
            observed: true
        }
    );
    assert!(registry.unresolved());
    assert!(registry.close_for_cancellation(operation).is_err());
    assert!(registry.close_for_charge_release(operation).is_err());
    assert!(registry.close_for_mutation(operation).is_err());
    drop(pending);
    assert!(registry.issue(operation, &shared).is_err());
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
}

#[test]
fn original_completion_conflict_keeps_first_evidence_sticky_and_closes_issuance() {
    for exact in [true, false] {
        let shared = shared();
        let mut registry = Registry::new();
        let mut pending = registry.issue(OperationId::new(), &shared).unwrap();
        pending.complete_before_effect([7; 32], true);
        pending.complete_before_effect([8; 32], exact);
        assert_eq!(
            *pending.fence.state.lock().unwrap(),
            State::UnknownCompletion { digest: [7; 32] }
        );
        registry.poll_completions();
        pending.complete_before_effect([7; 32], true);
        assert_eq!(
            *pending.fence.state.lock().unwrap(),
            State::UnknownCompletion { digest: [7; 32] }
        );
        assert!(!pending.enter());
        assert!(registry.issue(OperationId::new(), &shared).is_err());
    }
}

#[test]
fn original_completion_cannot_turn_entered_or_quiescent_execution_into_no_effect() {
    for quiescent in [false, true] {
        let shared = shared();
        let mut registry = Registry::new();
        let mut pending = registry.issue(OperationId::new(), &shared).unwrap();
        assert!(pending.enter());
        if quiescent {
            *pending.fence.state.lock().unwrap() = State::Unsettled;
        }
        pending.complete_before_effect([9; 32], true);
        assert_eq!(
            *pending.fence.state.lock().unwrap(),
            State::UnknownCompletion { digest: [9; 32] }
        );
        assert!(registry.unresolved());
    }
}

#[test]
fn original_completion_all_slots_retained_even_after_original_grants_drop() {
    let shared = shared();
    let mut registry = Registry::new();
    for byte in 0..32 {
        let pending = registry.issue(OperationId::new(), &shared).unwrap();
        pending.original_worker_started();
        pending.complete_before_effect([byte; 32], true);
        drop(pending);
    }
    registry.poll_completions();
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    assert_eq!(registry.slots.iter().flatten().count(), 32);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
}

#[test]
fn original_completion_worker_panic_and_zero_generation_never_become_closed_or_reusable() {
    let shared = shared();
    let mut registry = Registry::new();
    let pending = registry.issue(OperationId::new(), &shared).unwrap();
    let fence = pending.fence.clone();
    pending.original_worker_started();
    drop(pending); // Original worker unwind without a completion.
    assert_eq!(*fence.state.lock().unwrap(), State::Unknown);
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    let invalid = PendingFence {
        fence: Custody::new(Fence {
            operation: OperationId::new(),
            source: SourceRegistrationId::model_key_for_tests(0),
            generation: 0,
            incarnation: registry.incarnation,
            output: OnceLock::new(),
            state: Mutex::new(State::Pending),
            detail: Mutex::new(None),
            _backing: registry.input.as_ref().unwrap().clone(),
        }),
        entered: false,
    };
    invalid.complete_before_effect([3; 32], true);
    assert_eq!(
        *invalid.fence.state.lock().unwrap(),
        State::UnknownCompletion { digest: [3; 32] }
    );
}

#[test]
fn original_completion_worker_start_never_reopens_actor_closed_entry() {
    let shared = shared();
    let mut registry = Registry::new();
    let operation = OperationId::new();
    let mut pending = registry.issue(operation, &shared).unwrap();
    registry.close_for_cancellation(operation).unwrap();
    pending.original_worker_started();
    assert!(!pending.enter());
    pending.complete_before_effect([2; 32], true);
    assert!(registry.unresolved());
}

#[test]
fn original_completion_and_actor_closure_never_leave_executable_entry() {
    for _ in 0..64 {
        let shared = shared();
        let mut registry = Registry::new();
        let operation = OperationId::new();
        let mut pending = registry.issue(operation, &shared).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                barrier.wait();
                pending.original_worker_started();
                pending.complete_before_effect([7; 32], true);
            });
            barrier.wait();
            let _ = registry.close_for_cancellation(operation);
            worker.join().unwrap();
        });
        assert!(!pending.enter());
        registry.poll_completions();
        assert!(pending.completion_observed());
        assert!(registry.unresolved());
        assert!(registry.close_for_charge_release(operation).is_err());
    }
}

#[test]
fn original_completion_poison_preserves_first_digest_and_never_settles() {
    let shared = shared();
    let mut registry = Registry::new();
    let mut pending = registry.issue(OperationId::new(), &shared).unwrap();
    pending.complete_before_effect([7; 32], true);
    assert!(std::panic::catch_unwind(|| {
        let _lock = pending.fence.state.lock().unwrap();
        panic!("original completed slot poison");
    })
    .is_err());
    registry.poll_completions();
    assert_eq!(
        *pending.fence.state.lock().unwrap_err().into_inner(),
        State::UnknownCompletion { digest: [7; 32] }
    );
    pending.complete_before_effect([8; 32], true);
    assert_eq!(
        *pending.fence.state.lock().unwrap_err().into_inner(),
        State::UnknownCompletion { digest: [7; 32] }
    );
    assert!(!pending.enter());
    assert!(registry.unresolved());
    assert!(registry.issue(OperationId::new(), &shared).is_err());
}

#[test]
fn issuance_identity_observes_only_original_retained_slot_and_incarnation() {
    let shared = shared();
    let mut original = Registry::new();
    let mut foreign = Registry::new();
    let first = original.issue(OperationId::new(), &shared).unwrap();
    let second = original.issue(OperationId::new(), &shared).unwrap();
    let other = foreign.issue(OperationId::new(), &shared).unwrap();
    let a = original.issuance_observation(&first).unwrap();
    let b = original.issuance_observation(&second).unwrap();
    let c = foreign.issuance_observation(&other).unwrap();
    assert!(!a.0.is_nil());
    assert_eq!(a.0, b.0);
    assert_ne!(a.1, b.1);
    assert_ne!(a.0, c.0);
    assert!(original.issuance_observation(&other).is_none());
    assert!(foreign.issuance_observation(&first).is_none());
}

#[test]
fn publication_fence_custody_registry_drop_keeps_original_debit_until_last_worker() {
    let shared = shared();
    let mut registry = Registry::new();
    let mut workers: Vec<_> = (0..MAX_FENCES)
        .map(|_| registry.issue(OperationId::new(), &shared).unwrap())
        .collect();
    workers[0].original_worker_started();
    workers[0].complete_before_effect([7; 32], true);
    drop(registry);
    let charged = super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES;
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        charged
    );
    while workers.len() > 1 {
        drop(workers.pop());
        assert_eq!(
            shared.input_bytes.lock().unwrap().publication_remaining,
            charged
        );
    }
    drop(workers);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES
    );
}

#[cfg(unix)]
#[test]
fn publication_fence_custody_completion_observer_retains_original_allocation_debit() {
    let shared = shared();
    let mut registry = Registry::new();
    let pending = registry.issue(OperationId::new(), &shared).unwrap();
    let observe = pending.completion_observer();
    pending.original_worker_started();
    pending.complete_before_effect([9; 32], true);
    registry.poll_completions();
    drop(pending);
    drop(registry);
    assert!(observe());
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
    drop(observe);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES
    );
}

#[test]
fn publication_fence_custody_concurrent_last_worker_aliases_refund_exactly_once() {
    for _ in 0..64 {
        let shared = shared();
        let mut registry = Registry::new();
        let pending = registry.issue(OperationId::new(), &shared).unwrap();
        let first = pending.fence.clone();
        let second = pending.fence.clone();
        pending.original_worker_started();
        drop(pending); // Unknown worker termination remains uncertain, not reusable.
        drop(registry);
        assert_eq!(
            shared.input_bytes.lock().unwrap().publication_remaining,
            super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
        );
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first_drop = scope.spawn(|| {
                barrier.wait();
                drop(first);
            });
            barrier.wait();
            drop(second);
            first_drop.join().unwrap();
        });
        assert_eq!(
            shared.input_bytes.lock().unwrap().publication_remaining,
            super::super::PUBLICATION_INPUT_BYTES
        );
    }
}

#[test]
fn publication_fence_custody_backpressure_precedes_header_and_slot_allocation() {
    let shared = shared();
    let all = shared
        .reserve_publication_input(super::super::PUBLICATION_INPUT_BYTES)
        .unwrap();
    let mut registry = Registry::new();
    assert!(registry.issue(OperationId::new(), &shared).is_err());
    assert!(registry.input.is_none());
    assert!(registry.slots.iter().all(Option::is_none));
    assert_eq!(registry.next, 0);
    drop(all);
    let pending = registry.issue(OperationId::new(), &shared).unwrap();
    drop(pending);
    let replacement = registry.issue(OperationId::new(), &shared).unwrap();
    assert_eq!(registry.slots.iter().flatten().count(), 1);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
    drop(registry);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES - HEADER_BYTES
    );
    drop(replacement);
    assert_eq!(
        shared.input_bytes.lock().unwrap().publication_remaining,
        super::super::PUBLICATION_INPUT_BYTES
    );
}

#[test]
fn original_no_effect_slot_closes_only_after_exact_durable_callback_without_slot_lock() {
    let shared = shared();
    let mut registry = Registry::new();
    let operation = OperationId::new();
    let mut pending = registry.issue(operation, &shared).unwrap();
    pending.bind_reserved_output(uuid::Uuid::new_v4()).unwrap();
    pending.complete_before_effect([7; 32], true);
    let mut calls = 0;
    registry.settle_no_effects(|completion| {
        calls += 1;
        assert_eq!(completion.binding().0, operation);
        // This must be available while the actual store callback runs.
        assert!(pending.fence.state.try_lock().is_ok());
        assert!(registry.close_for_cancellation(operation).is_err());
        true
    });
    assert_eq!(calls, 1);
    assert!(!registry.unresolved());
    assert!(registry.close_for_cancellation(operation).is_ok());
    assert!(!pending.enter());
    pending.complete_before_effect([7; 32], true);
    assert!(!registry.unresolved());
    pending.complete_before_effect([8; 32], true);
    assert!(registry.unresolved());
    assert!(registry.close_for_mutation(operation).is_err());
}

#[test]
fn original_no_effect_callback_failure_or_completion_race_stays_uncertain() {
    for conflict in [false, true] {
        let shared = shared();
        let mut registry = Registry::new();
        let mut pending = registry.issue(OperationId::new(), &shared).unwrap();
        pending.bind_reserved_output(uuid::Uuid::new_v4()).unwrap();
        pending.complete_before_effect([7; 32], true);
        registry.settle_no_effects(|_| {
            if conflict {
                pending.complete_before_effect([8; 32], true);
            }
            conflict
        });
        assert!(registry.unresolved());
        assert!(!pending.enter());
        assert!(registry.issue(OperationId::new(), &shared).is_err());
    }
}
