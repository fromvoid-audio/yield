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

fn wait_retained_metrics(
    owner: &FileOperationOwner,
    receipts: u64,
    tombstones: u64,
) -> FileOwnerRetainedTelemetrySnapshot {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(snapshot) = owner.retained_telemetry_snapshot() {
            if snapshot.receipts == receipts && snapshot.tombstones == tombstones {
                return snapshot;
            }
        }
        assert!(
            Instant::now() < deadline,
            "original worker inventory observation"
        );
        thread::yield_now();
    }
}

#[test]
fn retained_metrics_original_lost_reply_duplicate_conflict_and_tombstone_custody() {
    use FileOwnerRetainedOutcome as Retained;
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "private root")
        .unwrap();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    owner.pause_before_execute_for_test();
    let original = owner.submit(command.clone_for_test()).unwrap();
    owner.wait_until_worker_paused_for_test();
    drop(original);
    owner.release_worker_for_test();
    let first = wait_retained_metrics(&owner, 1, 0);
    assert_eq!(first.primary_outcomes[Retained::Unsupported as usize], 1);
    assert_eq!(first.primary_outcomes.iter().sum::<u64>(), 1);
    let input = owner.input_bytes_remaining_for_test();
    let mut duplicate = owner.submit(command.clone_for_test()).unwrap();
    assert!(matches!(
        duplicate
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .outcome(),
        EffectOutcome::Unsupported { .. }
    ));
    let mut changed = command;
    let EffectRequest::FlushStaging { staging, .. } = &mut changed.effect else {
        panic!("fixture effect");
    };
    staging.binding.locator = BoundedRelativeLocator::new("foreign-change.bin").unwrap();
    let mut conflict = owner.submit(changed).unwrap();
    assert!(matches!(
        conflict
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .outcome(),
        EffectOutcome::Conflict { .. }
    ));
    assert_eq!(wait_retained_metrics(&owner, 1, 0), first);
    // The conflict reply's Drop also queues an acknowledgement. Complete that
    // original control command before observing the subsequent duplicate ack.
    acknowledge_with_bounded_retry(&mut conflict);
    acknowledge_with_bounded_retry(&mut duplicate);
    let tombstone = wait_retained_metrics(&owner, 0, 1);
    assert_eq!(tombstone.primary_outcomes, first.primary_outcomes);
    drop((duplicate, conflict));
    assert_eq!(owner.input_bytes_remaining_for_test(), input);
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    let cleared = wait_retained_metrics(&owner, 0, 0);
    assert_eq!(cleared.primary_outcomes, first.primary_outcomes);
    assert_eq!(
        owner.input_bytes_remaining_for_test(),
        FILE_OPERATION_OWNER_MAX_INPUT_BYTES
    );
}

#[test]
fn retained_metrics_original_cancel_and_panic_keep_typed_evidence() {
    use FileOwnerRetainedOutcome as Retained;
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    let identity = PendingEffectIdentity {
        operation_id: command.operation_id,
        participant_id: command.participant_id,
        effect_id: command.effect_id,
    };
    owner.pause_before_execute_for_test();
    let mut request = owner.submit(command).unwrap();
    owner.wait_until_worker_paused_for_test();
    assert_eq!(
        owner.cancel(identity),
        Ok(FileOperationCancelOutcome::KnownPending)
    );
    owner.release_worker_for_test();
    assert!(matches!(
        request
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .outcome(),
        EffectOutcome::SupersededBeforeEffect { .. }
    ));
    let before = wait_retained_metrics(&owner, 1, 0);
    assert_eq!(
        before.primary_outcomes[Retained::SupersededBeforeEffect as usize],
        1
    );
    assert_eq!(owner.execution_count_for_test(), 0);
    owner.panic_for_test().unwrap();
    drop(request);
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::RetirementUncertain { .. }
    ));
    assert_eq!(owner.retained_telemetry_snapshot().unwrap(), before);
    assert!(owner.pending_effects().contains(&identity));
}

#[test]
fn retained_metrics_actual_unavailable_outcome_is_not_clean_custody() {
    use FileOwnerRetainedOutcome as Retained;
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    owner.pause_before_execute_for_test();
    let mut request = owner
        .submit(unsupported_effect(&owner, registration, EffectId::new()))
        .unwrap();
    owner.wait_until_worker_paused_for_test();
    owner.shared.mark_unavailable("private retained reason");
    owner.release_worker_for_test();
    let result = request.recv_timeout(Duration::from_secs(2)).unwrap();
    // Already accepted work keeps the original Unsupported result; the
    // unavailable lifecycle independently prevents clean custody.
    assert!(matches!(
        result.outcome(),
        EffectOutcome::Unsupported { .. }
    ));
    let expected = Retained::Unsupported;
    let before = wait_retained_metrics(&owner, 1, 0);
    assert_eq!(before.primary_outcomes[expected as usize], 1);
    assert!(!format!("{before:?}").contains("private"));
    acknowledge_with_bounded_retry(&mut request);
    let tombstone = wait_retained_metrics(&owner, 0, 1);
    assert_eq!(tombstone.primary_outcomes, before.primary_outcomes);
    drop(request);
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::RetirementUncertain { .. }
    ));
    assert_eq!(owner.retained_telemetry_snapshot().unwrap(), tombstone);
}

#[test]
fn retained_metrics_original_lost_admission_boundary_retains_uncertainty() {
    use FileOwnerRetainedOutcome as Retained;
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    let command = unsupported_effect(&owner, registration, EffectId::new());
    let key = EffectKey {
        operation_id: command.operation_id,
        participant_id: command.participant_id,
        effect_id: command.effect_id,
    };
    owner.pause_before_execute_for_test();
    let mut request = owner.submit(command).unwrap();
    owner.wait_until_worker_paused_for_test();
    // Fault injection loses the actual original accepted scalar boundary; it
    // supplies no grant or fake positive effect. The real worker must retain it.
    owner.shared.admissions.lock().unwrap().remove(&key);
    owner
        .shared
        .mark_unavailable("lost original admission boundary");
    owner.release_worker_for_test();
    assert!(matches!(
        request
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .outcome(),
        EffectOutcome::RetainedUncertain { .. }
    ));
    let before = wait_retained_metrics(&owner, 1, 0);
    assert_eq!(
        before.primary_outcomes[Retained::RetainedUncertain as usize],
        1
    );
    assert_eq!(owner.execution_count_for_test(), 0);
    drop(request);
    assert!(matches!(
        owner.shutdown(),
        FileOperationOwnerShutdown::RetirementUncertain { .. }
    ));
    assert_eq!(
        owner
            .retained_telemetry_snapshot()
            .unwrap()
            .primary_outcomes,
        before.primary_outcomes
    );
}

#[cfg(unix)]
#[test]
fn retained_metrics_actual_readonly_staging_applied_is_only_observation() {
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::test_runtime::TestRuntimeGuard;
    use FileOwnerRetainedOutcome as Retained;
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("file-owner-retained-metrics");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner.register_profile_root(&guard).unwrap();
    let stage = guard.profile_root().join("stage.bin");
    std::fs::write(&stage, b"abc").unwrap();
    let command = FileOperationCommand::for_staging_validation(
        OperationId::new(),
        ParticipantId::new(),
        EffectId::new(),
        OutputIdentity::new(),
        registration,
        profile_fence(owner.owner_identity()),
        BoundedRelativeLocator::new("stage.bin").unwrap(),
        OwnedObjectIdentity::new(),
        StagingValidation::new(3, *blake3::hash(b"abc").as_bytes()),
        vec![],
        None,
    )
    .unwrap();
    let mut request = owner.submit(command).unwrap();
    assert!(matches!(
        request
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .outcome(),
        EffectOutcome::Applied { .. }
    ));
    let snapshot = wait_retained_metrics(&owner, 1, 0);
    assert_eq!(snapshot.primary_outcomes[Retained::Applied as usize], 1);
    assert_eq!(std::fs::read(&stage).unwrap(), b"abc");
    acknowledge_with_bounded_retry(&mut request);
    wait_retained_metrics(&owner, 0, 1);
    owner.acknowledge_custody_for_test();
    assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    assert_eq!(
        wait_retained_metrics(&owner, 0, 0).primary_outcomes,
        snapshot.primary_outcomes
    );
}

#[test]
fn retained_metrics_implicit_owner_drop_retains_original_inventory() {
    let owner = FileOperationOwner::start().unwrap();
    let registration = owner
        .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
        .unwrap();
    owner.pause_before_execute_for_test();
    let request = owner
        .submit(unsupported_effect(&owner, registration, EffectId::new()))
        .unwrap();
    owner.wait_until_worker_paused_for_test();
    drop(request);
    owner.release_worker_for_test();
    let before = wait_retained_metrics(&owner, 1, 0);
    let shared = Arc::clone(&owner.shared);
    drop(owner);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(outcome) = shared.shutdown_outcome.lock().unwrap().clone() {
            assert!(matches!(
                outcome,
                FileOperationOwnerShutdown::RetirementUncertain { .. }
            ));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "original implicit-drop worker completion"
        );
        thread::yield_now();
    }
    assert_eq!(shared.telemetry.retained_snapshot().unwrap(), before);
    let retained = shared.retained_state.lock().unwrap();
    let original = retained
        .as_ref()
        .expect("uncertain original owner state remains");
    assert_eq!((original.receipts.len(), original.tombstones.len()), (1, 0));
}
