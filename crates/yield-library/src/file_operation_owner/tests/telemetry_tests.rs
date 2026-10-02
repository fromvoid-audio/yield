use super::*;
use FileOwnerAdmissionOutcome as Outcome;

#[test]
fn telemetry_observes_actual_queue_backpressure_and_drain_without_control_labels() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "private root label")
        .unwrap();
    assert_eq!(
        owner.telemetry_snapshot(),
        FileOwnerTelemetrySnapshot::default()
    );
    owner.pause_before_execute_for_test();
    let mut requests = vec![
        owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .unwrap(),
    ];
    owner.wait_until_worker_paused_for_test();
    for _ in 0..FILE_OPERATION_OWNER_COMMAND_CAPACITY {
        requests.push(
            owner
                .submit(unsupported_effect(&owner, registration, EffectId::new()))
                .unwrap(),
        );
    }
    let rejection = owner.submit(unsupported_effect(&owner, registration, EffectId::new()));
    let waiting = owner.telemetry_snapshot();
    // Always release the worker before assertions so a regression cannot strand the test.
    owner.release_worker_for_test();
    assert!(matches!(rejection, Err(FileOperationOwnerQueueError::Full)));
    assert_eq!(
        waiting.queued_effects,
        FILE_OPERATION_OWNER_COMMAND_CAPACITY as u64
    );
    assert_eq!(
        waiting.peak_queued_effects,
        FILE_OPERATION_OWNER_COMMAND_CAPACITY as u64
    );
    assert_eq!(waiting.admissions[Outcome::Accepted as usize], 33);
    assert_eq!(waiting.admissions[Outcome::QueueFull as usize], 1);
    assert_eq!(waiting.dispatch_wait_buckets.iter().sum::<u64>(), 1);
    for request in &mut requests {
        assert!(matches!(
            request
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        acknowledge_with_bounded_retry(request);
    }
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    let drained = owner.telemetry_snapshot();
    assert_eq!(drained.queued_effects, 0);
    assert_eq!(drained.abandoned_effects, 0);
    assert_eq!(drained.dispatch_wait_buckets.iter().sum::<u64>(), 33);
    // Control registration, result acknowledgements and shutdown never enter effect metrics.
    assert_eq!(drained.admissions.iter().sum::<u64>(), 34);
}

#[test]
fn telemetry_counts_duplicate_submission_without_claiming_second_execution() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    let mut first = owner.submit(command.clone_for_test()).unwrap();
    let result = first.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut duplicate = owner.submit(command).unwrap();
    assert_eq!(
        duplicate.recv_timeout(Duration::from_secs(2)).unwrap(),
        result
    );
    let snapshot = owner.telemetry_snapshot();
    assert_eq!(snapshot.admissions[Outcome::Accepted as usize], 2);
    assert_eq!(snapshot.dispatch_wait_buckets.iter().sum::<u64>(), 2);
    assert_eq!(owner.execution_count_for_test(), 1);
    acknowledge_with_bounded_retry(&mut first);
    acknowledge_with_bounded_retry(&mut duplicate);
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
}

#[test]
fn telemetry_prequeue_budget_and_invalid_rejections_leave_no_backlog() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    *owner.shared.input_remaining.lock().unwrap() = 0;
    assert!(matches!(
        owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
        Err(FileOperationOwnerQueueError::InputBytesFull { .. })
    ));
    *owner.shared.input_remaining.lock().unwrap() = FILE_OPERATION_OWNER_MAX_INPUT_BYTES;
    *owner.shared.results_remaining.lock().unwrap() = 0;
    assert!(matches!(
        owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
        Err(FileOperationOwnerQueueError::ResultsFull)
    ));
    *owner.shared.results_remaining.lock().unwrap() = FILE_OPERATION_OWNER_RESULT_CAPACITY;
    let mut invalid = unsupported_effect(&owner, registration, EffectId::new());
    invalid.authorization = owner.test_observation_authorization(registration, vec![]);
    assert!(matches!(
        owner.submit(invalid),
        Err(FileOperationOwnerQueueError::InvalidInput(_))
    ));
    let snapshot = owner.telemetry_snapshot();
    assert_eq!(snapshot.admissions[Outcome::InputBytesFull as usize], 1);
    assert_eq!(snapshot.admissions[Outcome::ResultsFull as usize], 1);
    assert_eq!(snapshot.admissions[Outcome::InvalidInput as usize], 1);
    assert_eq!(snapshot.queued_effects, 0);
    assert_eq!(snapshot.peak_queued_effects, 0);
    assert_eq!(snapshot.dispatch_wait_buckets, [0; 6]);
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        FILE_OPERATION_OWNER_MAX_INPUT_BYTES
    );
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    assert!(matches!(
        owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
        Err(FileOperationOwnerQueueError::Stopped)
    ));
    assert_eq!(
        owner.telemetry_snapshot().admissions[Outcome::Stopped as usize],
        1
    );
}

#[test]
fn telemetry_panic_discards_queued_effects_without_dispatch_samples() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    owner.pause_before_execute_for_test();
    let first = owner
        .submit(unsupported_effect(&owner, registration, EffectId::new()))
        .unwrap();
    owner.wait_until_worker_paused_for_test();
    owner.panic_for_test().unwrap();
    let second = owner
        .submit(unsupported_effect(&owner, registration, EffectId::new()))
        .unwrap();
    owner.release_worker_for_test();
    drop(first);
    drop(second);
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::RetirementUncertain { .. }
    ));
    let snapshot = owner.telemetry_snapshot();
    assert_eq!(snapshot.queued_effects, 0);
    assert_eq!(snapshot.abandoned_effects, 1);
    assert_eq!(snapshot.dispatch_wait_buckets.iter().sum::<u64>(), 1);
    assert_eq!(snapshot.admissions[Outcome::Accepted as usize], 2);
    assert!(matches!(
        owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
        Err(FileOperationOwnerQueueError::Unavailable(_))
    ));
    assert_eq!(
        owner.telemetry_snapshot().admissions[Outcome::Unavailable as usize],
        1
    );
}
