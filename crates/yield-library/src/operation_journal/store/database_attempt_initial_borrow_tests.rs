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
