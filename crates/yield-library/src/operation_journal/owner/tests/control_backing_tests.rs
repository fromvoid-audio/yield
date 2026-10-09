use super::*;
use std::fs;

fn shared() -> Arc<OwnerShared> {
    let (commands, _receivers) = OwnerCommandReceivers::channel();
    Arc::new(OwnerShared::new(commands))
}

fn original_profile_journal_path(
    fixture: &Fixture,
    store: &OperationJournalStore,
) -> std::path::PathBuf {
    let path = fixture
        .owner
        .profile_root()
        .join(crate::operation_journal::JOURNAL_DATABASE_ROOT_NAME)
        .join(crate::operation_journal::JOURNAL_DATABASE_FILE_NAME);
    // The Store path can be a Linux /proc/self/fd alias, valid only while its
    // original descriptor lives. Verify the same original file bytes before
    // dispatch, then retain the actual profile namespace for post-join readback.
    assert_eq!(
        fs::read(&path).unwrap(),
        fs::read(store.retained_namespace_path_for_test()).unwrap()
    );
    path
}

fn exact_post_join_budget(owner: &OperationJournalOwner) -> InputBudget {
    let baseline = InputBudget::shared_initial();
    let mut live = baseline;
    live.remaining[0] -= resource_custody::HEADER_BYTES;
    live.publication_remaining -= resource_custody::HEADER_BYTES;
    assert_eq!(
        *owner.shared.input_bytes.lock().unwrap(),
        live,
        "the original runtime publication header is still alive before command submission"
    );
    baseline
}

#[test]
fn control_backing_last_original_alias_preserves_selected_debit_and_no_overwrite() {
    let fixture = fixture("control-backing-original-alias");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store.deny_original_control_begin_for_test();
    let path = original_profile_journal_path(&fixture, &store);
    let original = fs::read(&path).unwrap();
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let input = shared.startup_input().unwrap();
    let alias = input.historical_reply.as_ref().unwrap().clone();
    let error = store
        .rebuild_admission_snapshot_for_input(&input)
        .err()
        .unwrap();
    assert!(
        matches!(&error, OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(code, Some(message)))
        if code.extended_code == rusqlite::ffi::SQLITE_AUTH && message == "not authorized")
    );
    let selected = *shared.input_bytes.lock().unwrap();
    assert!(!input.can_retain_control_failure());
    assert!(input.can_retain_namespace_failure());
    assert!(matches!(
        store.rebuild_admission_snapshot_for_input(&input),
        Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert_eq!(*shared.input_bytes.lock().unwrap(), selected);
    drop(error);
    input.historical_error_consumed();
    assert_eq!(*shared.input_bytes.lock().unwrap(), selected);
    // Two terminal loans remain without an OwnerShared/reply cycle.
    let control_bytes = {
        let credits = alias.control.lock().unwrap();
        let credits = credits.as_ref().unwrap();
        credits.bytes_for_test()
    };
    drop(input);
    assert_eq!(
        baseline.remaining[0] - shared.input_bytes.lock().unwrap().remaining[0],
        control_bytes
            + HISTORICAL_REPLY_BYTES
            + crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
    );
    drop(alias);
    assert_eq!(*shared.input_bytes.lock().unwrap(), baseline);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn control_backing_slot_poison_invalidates_original_resource_and_lifecycle() {
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let input = shared.startup_input().unwrap();
    let alias = input.historical_reply.as_ref().unwrap().clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _slot = alias.control.lock().unwrap();
        panic!("original control slot poison");
    }));
    assert!(result.is_err());
    assert!(!input.can_retain_control_failure());
    assert_eq!(
        shared.lifecycle.load(Ordering::Acquire),
        OwnerLifecycle::Unavailable as u8
    );
    assert!(matches!(
        shared.resource_admission.validate_open(),
        Err(resource_custody::UseError::Unknown)
    ));
    drop(input);
    drop(alias);
    assert_eq!(*shared.input_bytes.lock().unwrap(), baseline);
}

#[test]
fn control_backing_queued_native_error_outlives_request_and_original_worker_join() {
    for poll in [false, true] {
        let fixture = fixture("control-backing-original-queued-error");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let path = original_profile_journal_path(&fixture, &store);
        let original = fs::read(&path).unwrap();
        store.deny_original_control_begin_for_test();
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let baseline = exact_post_join_budget(&owner);
        let pool = owner.shared.input_bytes.clone();
        let mut request = owner
            .client()
            .admit_prepared(prepared_copy_input(
                crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
                VolumeIdentity { device: 1 },
                1,
            ))
            .unwrap();
        let reply_probe = request.historical_reply.as_ref().unwrap().clone();
        let result = if poll {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match request.try_result() {
                    Ok(result) => break result,
                    Err(OperationJournalRequestError::Empty) => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    other => panic!("original queued control result disconnected: {other:?}"),
                }
            }
        } else {
            request.recv_timeout(Duration::from_secs(5)).unwrap()
        };
        let error = result.unwrap_err();
        let OperationJournalOwnerError::RetainedBacking(ref backed) = error else {
            panic!("original selected control debit must follow caller error");
        };
        assert!(
            matches!(backed.original_error(), OperationJournalOwnerError::PreparedAdmission(
            PreparedAdmissionError::RecoveryRequired { reason })
            if reason == "operation journal SQLite error: not authorized")
        );
        let control_bytes = reply_probe
            .control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .bytes_for_test();
        let consumer_bytes = reply_probe
            .consumer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .historical_test_funded_bytes();
        assert_eq!(
            consumer_bytes,
            super::super::super::store::HISTORICAL_DIAGNOSTIC_BYTES
        );
        assert!(reply_probe.observer.lock().unwrap().is_none());
        assert!(reply_probe.namespace.lock().unwrap().is_none());
        drop(reply_probe);
        drop(request);
        assert!(owner.shutdown());
        assert_eq!(owner.prepared_admission_id_allocations(), 0);
        assert_eq!(
            baseline.remaining[0] - pool.lock().unwrap().remaining[0],
            control_bytes + consumer_bytes + HISTORICAL_REPLY_BYTES
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        drop(error);
        assert_eq!(*pool.lock().unwrap(), baseline);
    }
}

#[test]
fn control_backing_queued_lost_reply_refunds_after_original_worker_join() {
    let fixture = fixture("control-backing-original-lost-reply");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let path = original_profile_journal_path(&fixture, &store);
    let original = fs::read(&path).unwrap();
    store.deny_original_control_begin_for_test();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let baseline = exact_post_join_budget(&owner);
    let request = owner
        .client()
        .admit_prepared(prepared_copy_input(
            crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        ))
        .unwrap();
    drop(request);
    assert!(owner.shutdown());
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    assert_eq!(*owner.shared.input_bytes.lock().unwrap(), baseline);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn control_backing_original_startup_component_handoffs_native_diagnostic_then_refunds() {
    let fixture = fixture("control-backing-original-startup-component");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let path = original_profile_journal_path(&fixture, &store);
    let original = fs::read(&path).unwrap();
    store.deny_original_control_begin_for_test();
    let (commands, receivers) = OwnerCommandReceivers::channel();
    let shared = Arc::new(OwnerShared::new(commands));
    shared.startup_component.store(true, Ordering::Release);
    let baseline = *shared.input_bytes.lock().unwrap();
    let pool = shared.input_bytes.clone();
    let ids = Arc::new(AtomicUsize::new(0));
    let copies = ids.clone();
    let worker_shared = shared.clone();
    let worker = std::thread::spawn(move || {
        run_owner(
            store,
            receivers,
            worker_shared,
            None,
            false,
            false,
            None,
            copies,
            Arc::new(AtomicUsize::new(0)),
        )
    });
    worker.join().expect("original startup worker join");
    assert_eq!(ids.load(Ordering::Acquire), 0);
    assert_eq!(
        shared.lifecycle.load(Ordering::Acquire),
        OwnerLifecycle::Unavailable as u8
    );
    assert_eq!(
        shared.startup_component_claims.load(Ordering::Acquire),
        usize::MAX
    );
    assert_eq!(
        shared.unavailable.lock().unwrap().as_deref(),
        Some("operation journal SQLite error: not authorized")
    );
    assert!(
        pool.lock().unwrap().remaining[0] < baseline.remaining[0],
        "Shared diagnostic remains under its original detached debit after worker join"
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    drop(shared);
    assert_eq!(*pool.lock().unwrap(), baseline);
}

#[cfg(target_os = "macos")]
#[test]
fn control_backing_native_production_startup_preserves_original_failure_and_journal() {
    let fixture = fixture("control-backing-native-production-startup");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let path = original_profile_journal_path(&fixture, &store);
    let original = fs::read(&path).unwrap();
    store.deny_original_reader_begin_for_test();
    // Genuine original production registry, root descriptors and protected
    // profile/journal reserve bootstrap. The only fault is SQLite's authorizer
    // on this same original connection. No startup/provider component hook.
    let mut owner = OperationJournalOwner::start(store).unwrap();
    let pool = owner.shared.input_bytes.clone();
    let baseline = InputBudget::shared_initial();
    let reason = wait_until_unavailable(&owner);
    assert_eq!(
        reason,
        "ordinary-claim capacity rebuild bootstrap failed: operation journal SQLite error: not authorized"
    );
    assert!(!owner.shared.startup_component.load(Ordering::Acquire));
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    assert!(owner.shutdown());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(
        pool.lock().unwrap().remaining[0] < baseline.remaining[0],
        "original Shared diagnostic backing remains after actual worker join"
    );
    drop(owner);
    assert_eq!(*pool.lock().unwrap(), baseline);
}
