// Actual original SQLite row reads; diagnostics confer no journal/effect authority.
#[test]
fn journal_point_row_oversized_blob_refuses_before_copy_and_preserves_exact_bytes() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let operation = OperationId::new();
    let length = MAX_RECORD_BYTES + 1;
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO operation_journal VALUES (?1,1,zeroblob(?2))",
            params![operation.to_string(), length as i64],
        )
        .unwrap();
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", false)
        .unwrap();
    for _ in 0..2 {
        assert!(
            matches!(store.get(operation),Err(OperationJournalStoreError::RetainedRecord{reason,..}) if reason.contains("length"))
        );
        assert_eq!(
            POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.get()),
            0,
            "oversized original BLOB must not reach Rust byte materialization"
        );
        assert!(store.connection.is_autocommit());
    }
    let actual: (String, i64, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1",
            [operation.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(actual, (operation.to_string(), 1, vec![0; length]));
    drop(store);
    let reopened = OperationJournalStore::open(&f.owner).unwrap();
    assert!(reopened.get(operation).is_err());
    assert_eq!(POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.get()), 0);
}

#[test]
fn journal_point_row_wrong_storage_types_are_preserved_without_blob_projection() {
    for value in [
        rusqlite::types::Value::Integer(7),
        rusqlite::types::Value::Real(3.5),
        rusqlite::types::Value::Text("unknown retained record".into()),
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let operation = OperationId::new();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1,1,?2)",
                params![operation.to_string(), value],
            )
            .unwrap();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        assert!(
            matches!(store.get(operation),Err(OperationJournalStoreError::RetainedRecord{reason,..}) if reason.contains("BLOB"))
        );
        assert_eq!(POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.get()), 0);
        let retained: rusqlite::types::Value = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id=?1",
                [operation.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, value);
        assert!(store.connection.is_autocommit());
    }
}

#[test]
fn journal_point_row_keeps_full_existing_bound_and_valid_record_readback() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let operation = OperationId::new();
    let record = intent_record(operation);
    store.insert_intent(&record).unwrap();
    assert_eq!(store.get(operation).unwrap(), Some(record.clone()));
    assert_eq!(
        POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.get()),
        encode(&record).unwrap().len()
    );
    store
        .connection
        .execute(
            "UPDATE operation_journal SET record=zeroblob(?1) WHERE operation_id=?2",
            params![MAX_RECORD_BYTES as i64, operation.to_string()],
        )
        .unwrap();
    assert!(store.get(operation).is_err()); // Bounded unknown bytes are retained codec failures.
    assert_eq!(
        POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.get()),
        MAX_RECORD_BYTES
    );
    let retained: i64 = store
        .connection
        .query_row(
            "SELECT length(record) FROM operation_journal WHERE operation_id=?1",
            [operation.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, MAX_RECORD_BYTES as i64);
}
