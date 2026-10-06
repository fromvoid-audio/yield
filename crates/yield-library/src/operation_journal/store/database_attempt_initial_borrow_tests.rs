// Actual original Store paths; logical attempt observations do not issue permits.
#[test]
fn database_attempt_initial_borrow_preserves_first_stored_start() {
    use crate::operation_journal::database_attempts::record_backing::{
        HISTORICAL_LABEL_CAPACITY, INITIAL_LABEL_CAPACITY,
    };
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (mut record, claims, inventory) =
        integration::actual_journal_input_count(&store, &f.owner, 1);
    record.intent.label = "x".repeat(crate::operation_journal::model::MAX_UNIVERSAL_LABEL_BYTES);
    let provider = logical_provider(&record);
    assert!(
        settle_fixture(&store, &record, &claims, &inventory, &provider)
            .outcome()
            .receipt()
            .is_some()
    );
    let stored = store.read_row(record.operation_id).unwrap().unwrap().record;
    assert_eq!(stored, record);
    let a = start(&stored, &inventory, 0);
    let image = a.encode().unwrap();
    let digest = a.digest().unwrap();
    INITIAL_LABEL_CAPACITY.with(|n| n.set(0));
    HISTORICAL_LABEL_CAPACITY.with(|n| n.set(0));
    assert!(!store.persist_database_attempt_start(&a).unwrap().replay);
    assert_eq!(
        INITIAL_LABEL_CAPACITY.with(|n| n.get()),
        0,
        "initial validation duplicated original owned label backing"
    );
    assert!(HISTORICAL_LABEL_CAPACITY.with(|n| n.get()) >= stored.intent.label.len());
    assert_eq!(
        store.read_database_attempt(a.attempt).unwrap().unwrap(),
        (a.clone(), None)
    );
    assert_eq!(a.encode().unwrap(), image);
    assert_eq!(a.digest().unwrap(), digest);
    assert_eq!(
        store.read_row(record.operation_id).unwrap().unwrap().record,
        stored
    );
    assert!(store.connection.is_autocommit());
}

#[test]
fn database_attempt_initial_borrow_preserves_original_validation_order() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let stored = store.read_row(record.operation_id).unwrap().unwrap().record;
    let a = start(&stored, &inventory, 0);
    let before = retained_image(&store);
    for phase in [
        OperationPhase::IntentDurable,
        OperationPhase::SourceReconciled,
        OperationPhase::Terminal,
    ] {
        for disposition in [
            OperationDisposition::None,
            OperationDisposition::RetryPending,
            OperationDisposition::CancelRequestedBeforePublish,
        ] {
            for initial in [true, false] {
                for fault in 0..4 {
                    let mut actual = stored.clone();
                    actual.phase = phase;
                    actual.disposition = disposition;
                    if fault == 1 {
                        actual.intent.label.clear();
                    }
                    let mut candidate = a.clone();
                    if fault == 2 {
                        candidate.database_bytes = u64::MAX;
                    }
                    if fault == 3 {
                        candidate.nonce = Uuid::nil();
                        candidate.database_slice = Uuid::nil();
                    }
                    let mut original = actual.clone();
                    if !initial {
                        original.phase = candidate.phase;
                        original.disposition = candidate.disposition;
                    }
                    let expected = DatabaseAttemptStart::from_context(&original, &inventory, candidate.attempt, candidate.nonce, candidate.database_slice, candidate.sidecar_slice, candidate.database_bytes, candidate.wal_bytes, candidate.shm_bytes)
                        .map_err(|e| e.to_string())
                        .and_then(|expected| if expected == candidate { Ok(()) } else { Err("unsupported database claim inventory: attempt differs from actual immutable intent/binding/phase".into()) });
                    assert_eq!(
                        candidate
                            .validate_bound(&actual, &inventory, initial)
                            .map_err(|e| e.to_string()),
                        expected,
                        "phase={phase:?} disposition={disposition:?} initial={initial} fault={fault}"
                    );
                }
            }
        }
    }
    assert_eq!(retained_image(&store), before);
    assert!(store.connection.is_autocommit());
}

#[test]
fn database_attempt_initial_borrow_keeps_historical_overlay_backing() {
    use crate::operation_journal::database_attempts::record_backing::{
        HISTORICAL_LABEL_CAPACITY, INITIAL_LABEL_CAPACITY,
    };
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let mut stored = store.read_row(record.operation_id).unwrap().unwrap().record;
    let a = start(&stored, &inventory, 0);
    stored.phase = OperationPhase::Terminal;
    stored.disposition = OperationDisposition::CancelRequestedBeforePublish;
    INITIAL_LABEL_CAPACITY.with(|n| n.set(0));
    HISTORICAL_LABEL_CAPACITY.with(|n| n.set(0));
    a.validate_bound(&stored, &inventory, false).unwrap();
    assert_eq!(INITIAL_LABEL_CAPACITY.with(|n| n.get()), 0);
    assert!(HISTORICAL_LABEL_CAPACITY.with(|n| n.get()) >= stored.intent.label.len());
    assert_eq!(stored.phase, OperationPhase::Terminal);
    assert_eq!(
        stored.disposition,
        OperationDisposition::CancelRequestedBeforePublish
    );
    assert_eq!(a.phase, OperationPhase::IntentDurable);
    assert_eq!(a.disposition, OperationDisposition::None);
}
