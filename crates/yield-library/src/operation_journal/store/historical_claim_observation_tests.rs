#[test]
fn historical_claim_sqlite_borrowed_observer_preserves_original_lookup_and_full_rebuild() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let operation_id = OperationId::new();
    let mut record = intent_record(operation_id);
    // A dense opaque payload is a valid original 1 MiB wire image, not a new schema limit.
    record.payload = serde_json::Value::Null;
    let encoded = encode(&record).unwrap();
    let original = String::from_utf8(encoded).unwrap();
    let suffix = ",0".repeat((MAX_RECORD_BYTES - original.len() - 3) / 2);
    let wire = original.replace("\"payload\": null", &format!("\"payload\": [0{suffix}]"));
    assert_ne!(wire, original);
    assert!(wire.len() <= MAX_RECORD_BYTES);
    store
        .connection
        .execute(
            "INSERT INTO operation_journal VALUES (?1,1,?2)",
            params![operation_id.to_string(), wire.as_bytes()],
        )
        .unwrap();
    insert_ordinary_claim(
        &store,
        Uuid::new_v4(),
        operation_id.as_uuid(),
        "1",
        "DestinationStaging",
        1,
    )
    .unwrap();
    let input = super::super::owner::InputPermit::historical_test_input(1);
    let available = input.historical_test_remaining();
    assert!(store.observe_claim_operation(operation_id, &input).unwrap());
    assert_eq!(input.historical_test_remaining(), available);
    let funded = store.rebuild_admission_snapshot_for_input(&input).unwrap();
    let old = store.rebuild_admission_snapshot().unwrap();
    assert_eq!(funded, old);
    assert_eq!(input.historical_test_remaining(), available);
    assert!(
        store.get(operation_id).unwrap().unwrap().payload.is_array(),
        "public get retains the original payload"
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT record FROM operation_journal", [], |row| row
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        wire.as_bytes()
    );
}

#[test]
fn historical_claim_sqlite_errors_and_pressure_preserve_original_rows_and_credit() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let operation_id = OperationId::new();
    let record = intent_record(operation_id);
    let wire = String::from_utf8(encode(&record).unwrap())
        .unwrap()
        .replace("\"payload\": {}", r#""payload": {"a":0,"\u0061":1}"#);
    store
        .connection
        .execute(
            "INSERT INTO operation_journal VALUES (?1,1,?2)",
            params![operation_id.to_string(), wire.as_bytes()],
        )
        .unwrap();
    let input = super::super::owner::InputPermit::historical_test_input(1);
    let available = input.historical_test_remaining();
    assert!(store.observe_claim_operation(operation_id, &input).is_err());
    assert!(store.get(operation_id).is_err());
    assert_eq!(input.historical_test_remaining(), available);
    let pressure = super::super::owner::InputPermit::historical_test_input(3 * MAX_RECORD_BYTES);
    let available = pressure.historical_test_remaining();
    let huge = format!("{}{}", " ".repeat(MAX_RECORD_BYTES - wire.len()), wire);
    store
        .connection
        .execute(
            "UPDATE operation_journal SET record=?1",
            params![huge.as_bytes()],
        )
        .unwrap();
    assert!(matches!(
        store.observe_claim_operation(operation_id, &pressure),
        Err(OperationJournalStoreError::HistoricalClaimResourceBound)
    ));
    assert_eq!(pressure.historical_test_remaining(), available);
    assert_eq!(
        store
            .connection
            .query_row("SELECT record FROM operation_journal", [], |row| row
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        huge.as_bytes()
    );
}
