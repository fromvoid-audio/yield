use super::*;

fn poll_to_completion(request: &mut FileOperationRequest) -> Arc<FileOperationResult> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = request.try_result().expect("original result channel") {
            return result;
        }
        assert!(Instant::now() < deadline, "original owner did not reply");
        thread::yield_now();
    }
}

#[test]
fn nonblocking_poll_keeps_pending_custody_and_consumes_shared_receipt_once() {
    let owner = FileOperationOwner::start().expect("original owner");
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    owner.pause_before_execute_for_test();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    let mut first = owner.submit(command.clone_for_test()).unwrap();
    owner.wait_until_worker_paused_for_test();
    let mut duplicate = owner.submit(command.clone_for_test()).unwrap();
    let remaining = owner.input_bytes_remaining_for_test();
    // Release the actual worker before assertions, so a failing assertion cannot
    // strand shutdown. Both observations were made while execution was paused.
    let empty_first = first.try_result();
    let empty_duplicate = duplicate.try_result();
    let premature_ack = first.acknowledge();
    let remaining_after_poll = owner.input_bytes_remaining_for_test();
    let pending_after_poll = owner.pending_effects();
    owner.release_worker_for_test();
    assert!(matches!(empty_first, Ok(None)));
    assert!(matches!(empty_duplicate, Ok(None)));
    assert!(matches!(
        premature_ack,
        Err(FileOperationRequestError::Acknowledgement(_))
    ));
    assert_eq!(remaining_after_poll, remaining);
    assert_eq!(pending_after_poll.len(), 1);

    let first_result = poll_to_completion(&mut first);
    let duplicate_result = poll_to_completion(&mut duplicate);
    assert!(Arc::ptr_eq(&first_result, &duplicate_result));
    assert_eq!(first_result.operation_id(), command.operation_id);
    assert_eq!(first_result.effect_id(), command.effect_id);
    assert!(matches!(
        first_result.outcome(),
        EffectOutcome::Unsupported { .. }
    ));
    assert_eq!(owner.execution_count_for_test(), 1);
    assert!(matches!(
        first.try_result(),
        Err(FileOperationRequestError::AlreadyConsumed)
    ));
    assert!(matches!(
        first.recv(),
        Err(FileOperationRequestError::AlreadyConsumed)
    ));
    acknowledge_with_bounded_retry(&mut first);
    acknowledge_with_bounded_retry(&mut duplicate);
    drop(first);
    drop(duplicate);
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
}

#[test]
fn dropping_unconsumed_poll_handle_keeps_original_receipt_for_exact_retry() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    owner.pause_before_execute_for_test();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    let mut lost = owner.submit(command.clone_for_test()).unwrap();
    owner.wait_until_worker_paused_for_test();
    let empty = lost.try_result();
    drop(lost);
    let pending = owner.pending_effects();
    owner.release_worker_for_test();
    assert!(matches!(empty, Ok(None)));
    assert_eq!(pending.len(), 1);
    let mut retry = owner.submit(command).unwrap();
    let receipt = poll_to_completion(&mut retry);
    assert!(matches!(
        receipt.outcome(),
        EffectOutcome::Unsupported { .. }
    ));
    assert_eq!(owner.execution_count_for_test(), 1);
    acknowledge_with_bounded_retry(&mut retry);
    drop(retry);
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
}
