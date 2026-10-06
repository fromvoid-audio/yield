use super::*;

#[test]
fn original_root_backing_lost_reply_and_original_worker_teardown_preserve_exact_credit() {
    let owner = FileOperationOwner::start().unwrap();
    let root = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "barrier root")
        .unwrap();
    let baseline = owner.input_bytes_remaining_for_test();
    let registration = RootRegistrationRef {
        owner: owner.owner_identity(),
        registration_id: Uuid::new_v4(),
        generation: 1,
    };
    let label = "lost original reply";
    let input = owner
        .shared
        .reserve_input(
            std::mem::size_of::<OwnerCommand>()
                + label.len()
                + FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES,
        )
        .unwrap();
    let (result, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    owner
        .shared
        .try_control(OwnerCommand::RegisterRoot {
            registration,
            authority: profile_fence(owner.owner_identity()),
            label: label.to_owned(),
            capability: None,
            input: Some(input),
            result,
        })
        .unwrap();
    // The original actor processes this duplicate only after the abandoned reply.
    assert!(
        owner
            .register_root(root, profile_fence(owner.owner_identity()), "barrier", None)
            .is_err()
    );
    assert_eq!(owner.input_bytes_remaining_for_test(), baseline);
    assert!(owner.retire_root(registration).is_err());
    owner.panic_for_test().unwrap();
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::RetirementUncertain { .. }
    ));
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        FILE_OPERATION_OWNER_MAX_INPUT_BYTES
    );
}

#[test]
fn original_root_backing_rejects_exhaustion_before_registration_and_restores_exact_baseline() {
    let owner = FileOperationOwner::start().unwrap();
    let baseline = owner.input_bytes_remaining_for_test();
    assert!(
        baseline < FILE_OPERATION_OWNER_MAX_INPUT_BYTES,
        "fixed original slots are prepaid"
    );
    let credit = owner.shared.reserve_input(baseline).unwrap();
    assert!(
        owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "exhausted")
            .is_err()
    );
    assert_eq!(owner.input_bytes_remaining_for_test(), 0);
    drop(credit);
    assert_eq!(owner.input_bytes_remaining_for_test(), baseline);
    let root = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "retained")
        .unwrap();
    let retained = owner.input_bytes_remaining_for_test();
    assert!(retained < baseline);
    assert!(
        owner
            .register_root(
                root,
                profile_fence(owner.owner_identity()),
                "duplicate",
                None
            )
            .is_err()
    );
    assert_eq!(owner.input_bytes_remaining_for_test(), retained);
    owner.retire_root(root).unwrap();
    assert_eq!(owner.input_bytes_remaining_for_test(), baseline);
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::DrainedAck
    ));
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        FILE_OPERATION_OWNER_MAX_INPUT_BYTES
    );
}

#[test]
fn original_root_liveness_last_alias_refunds_only_after_closed_allocation_is_destroyed() {
    let owner = FileOperationOwner::start().unwrap();
    let baseline = owner.input_bytes_remaining_for_test();
    let original = RootLiveness::new(&owner.shared).unwrap();
    let alias = original.clone();
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        baseline - RootLiveness::BACKING_BYTES
    );
    original.store(false, Ordering::Release);
    drop(original);
    assert!(!alias.load(Ordering::Acquire));
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        baseline - RootLiveness::BACKING_BYTES
    );
    drop(alias);
    assert_eq!(owner.input_bytes_remaining_for_test(), baseline);
}

#[test]
fn original_root_liveness_concurrent_last_aliases_refund_exactly_once() {
    let owner = FileOperationOwner::start().unwrap();
    let baseline = owner.input_bytes_remaining_for_test();
    let first = RootLiveness::new(&owner.shared).unwrap();
    let second = first.clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        other.wait();
        drop(second);
    });
    barrier.wait();
    drop(first);
    worker.join().unwrap();
    assert_eq!(owner.input_bytes_remaining_for_test(), baseline);
}

#[test]
fn original_root_backing_queued_unobserved_success_retains_custody_until_worker_join() {
    let owner = FileOperationOwner::start().unwrap();
    let barrier = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "barrier")
        .unwrap();
    let baseline = owner.input_bytes_remaining_for_test();
    let registration = RootRegistrationRef {
        owner: owner.owner_identity(),
        registration_id: Uuid::new_v4(),
        generation: 1,
    };
    let label = "queued unobserved success";
    let input = owner
        .shared
        .reserve_input(
            std::mem::size_of::<OwnerCommand>()
                + label.len()
                + FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES,
        )
        .unwrap();
    let (result, receiver) = mpsc::sync_channel(1);
    owner
        .shared
        .try_control(OwnerCommand::RegisterRoot {
            registration,
            authority: profile_fence(owner.owner_identity()),
            label: label.to_owned(),
            capability: None,
            input: Some(input),
            result,
        })
        .unwrap();
    // FIFO duplicate response proves the original worker already queued success.
    assert!(
        owner
            .register_root(
                barrier,
                profile_fence(owner.owner_identity()),
                "barrier",
                None
            )
            .is_err()
    );
    let retained = owner.input_bytes_remaining_for_test();
    assert!(retained < baseline);
    drop(receiver);
    assert!(
        owner
            .register_root(
                barrier,
                profile_fence(owner.owner_identity()),
                "barrier",
                None
            )
            .is_err()
    );
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        retained,
        "unknown queued outcome cannot return live registration backing"
    );
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::DrainedAck
    ));
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        FILE_OPERATION_OWNER_MAX_INPUT_BYTES
    );
}
