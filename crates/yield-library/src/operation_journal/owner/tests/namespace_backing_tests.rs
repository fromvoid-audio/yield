use super::*;
use std::fs;

fn shared() -> Arc<OwnerShared> {
    let (commands, _receivers) = OwnerCommandReceivers::channel();
    Arc::new(OwnerShared::new(commands))
}

#[test]
fn namespace_backing_same_original_store_preserves_outcome_and_selected_debit() {
    let fixture = fixture("namespace-backing-original-parity");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let path = store.retained_namespace_path_for_test();
    let original = fs::read(&path).unwrap();
    for scenario in ["clean", "active", "wal", "restored"] {
        if scenario == "active" {
            store.retained_namespace_transaction_for_test(true).unwrap();
        }
        let wal = path.with_file_name("journal.sqlite3-wal");
        if scenario == "wal" {
            fs::write(&wal, b"same retained sidecar evidence").unwrap();
        }
        let old = store.retained_namespace_allocation_probe_for_test(false);
        let shared = shared();
        let baseline = *shared.input_bytes.lock().unwrap();
        let input = shared.startup_input().unwrap();
        let available = input.historical_test_remaining();
        let funded = store.validate_namespace_with_input(Some(&input), false);
        let bound = store.retained_namespace_working_bytes_for_test();
        match (old, funded) {
            (Ok(()), Ok(())) => assert_eq!(input.historical_test_remaining(), available),
            (Err(old), Err(funded)) => {
                assert_eq!(
                    std::mem::discriminant(&old),
                    std::mem::discriminant(&funded)
                );
                assert_eq!(old.to_string(), funded.to_string(), "{scenario}");
                assert_eq!(input.historical_test_remaining(), available - bound);
                drop(funded);
                assert_eq!(
                    input.historical_test_remaining(),
                    available - bound,
                    "original diagnostic consumer still owns selected backing"
                );
            }
            _ => panic!("same original namespace outcome changed: {scenario}"),
        }
        drop(input);
        assert_eq!(*shared.input_bytes.lock().unwrap(), baseline);
        if scenario == "active" {
            store
                .retained_namespace_transaction_for_test(false)
                .unwrap();
        }
        if scenario == "wal" {
            assert_eq!(fs::read(&wal).unwrap(), b"same retained sidecar evidence");
            fs::remove_file(wal).unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn namespace_backing_exact_pressure_precedes_original_path_error() {
    let fixture = fixture("namespace-backing-strict-pressure");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let original_path = store.retained_namespace_path_for_test();
    let original_bytes = fs::read(&original_path).unwrap();
    store.retained_namespace_transaction_for_test(true).unwrap();
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let input = shared.startup_input().unwrap();
    let bound = store.retained_namespace_working_bytes_for_test();
    let held = shared
        .reserve_input(
            InputClass::Ordinary,
            input.historical_test_remaining() - bound + 1,
        )
        .unwrap();
    let before = *shared.input_bytes.lock().unwrap();
    assert!(matches!(
        store.validate_namespace_with_input(Some(&input), false),
        Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert_eq!(*shared.input_bytes.lock().unwrap(), before);
    assert!(input.can_retain_namespace_failure());
    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
    drop(held);
    let error = store
        .validate_namespace_with_input(Some(&input), false)
        .unwrap_err();
    assert!(error.to_string().contains("active transaction"));
    drop(error);
    drop(input);
    assert_eq!(*shared.input_bytes.lock().unwrap(), baseline);
    store
        .retained_namespace_transaction_for_test(false)
        .unwrap();
}

#[test]
fn namespace_backing_last_original_reply_alias_and_no_overwrite() {
    let fixture = fixture("namespace-backing-reply-custody");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store.retained_namespace_transaction_for_test(true).unwrap();
    let shared = shared();
    let baseline = *shared.input_bytes.lock().unwrap();
    let input = shared.startup_input().unwrap();
    let alias = input.historical_reply.as_ref().unwrap().clone();
    let error = store
        .validate_namespace_with_input(Some(&input), false)
        .unwrap_err();
    let after = *shared.input_bytes.lock().unwrap();
    assert!(matches!(
        store.validate_namespace_with_input(Some(&input), false),
        Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert_eq!(*shared.input_bytes.lock().unwrap(), after);
    drop(error);
    input.historical_error_consumed();
    assert_eq!(
        *shared.input_bytes.lock().unwrap(),
        after,
        "observer consumption cannot release original namespace error backing"
    );
    let bound = store.retained_namespace_working_bytes_for_test();
    drop(input);
    assert_eq!(
        baseline.remaining[0] - shared.input_bytes.lock().unwrap().remaining[0],
        bound
            + HISTORICAL_REPLY_BYTES
            + crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
    );
    drop(alias);
    assert_eq!(*shared.input_bytes.lock().unwrap(), baseline);
    store
        .retained_namespace_transaction_for_test(false)
        .unwrap();
}

#[test]
fn namespace_backing_missing_consumer_refuses_before_original_namespace() {
    let fixture = fixture("namespace-backing-missing-consumer");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store.retained_namespace_transaction_for_test(true).unwrap();
    let input = InputPermit::historical_test_input(1);
    let available = input.historical_test_remaining();
    assert!(matches!(
        store.validate_namespace_with_input(Some(&input), false),
        Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert_eq!(input.historical_test_remaining(), available);
    store
        .retained_namespace_transaction_for_test(false)
        .unwrap();
}
