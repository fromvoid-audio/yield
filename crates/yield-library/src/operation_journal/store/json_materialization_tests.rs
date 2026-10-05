// Stored observations only: this exercises no physical qualification or admission authority.
fn stored_json_limits() -> crate::operation_journal::codec::JsonMaterializationLimits {
    use crate::operation_journal::MAX_RECORD_BYTES;
    use crate::operation_journal::codec::JsonMaterializationLimits;
    JsonMaterializationLimits {
        maximum_nodes: MAX_RECORD_BYTES,
        maximum_array_entries: MAX_RECORD_BYTES,
        maximum_object_entries: MAX_RECORD_BYTES,
        maximum_owned_string_bytes: MAX_RECORD_BYTES,
        maximum_depth: 128,
    }
}

#[test]
fn stored_json_materialization_refusal_preserves_actual_bytes_key_sequence_and_reopen() {
    use crate::operation_journal::codec::{
        BoundedDecodeError, JsonResourceKind, decode_with_limits,
    };
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut record = intent_record(OperationId::new());
    record.payload = serde_json::json!(vec![serde_json::Value::Null; 10_000]);
    let receipt = store.insert_intent(&record).unwrap();
    let retained = |store: &OperationJournalStore| -> (String, i64, Vec<u8>) {
        store.connection.query_row(
            "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id=?1",
            [record.operation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap()
    };
    let before = retained(&store);
    assert_eq!(before.0, record.operation_id.to_string());
    assert_eq!(before.1, receipt.sequence() as i64);
    assert_eq!(before.2, encode(&record).unwrap());
    assert!(matches!(
        decode_with_limits(
            &before.2,
            crate::operation_journal::codec::JsonMaterializationLimits {
                maximum_array_entries: 16,
                ..stored_json_limits()
            }
        ),
        Err(BoundedDecodeError::LimitExceeded {
            resource: JsonResourceKind::ArrayEntries,
            maximum: 16
        })
    ));
    assert!(store.connection.is_autocommit());
    assert_eq!(retained(&store), before);
    assert_eq!(
        store.get(record.operation_id).unwrap(),
        Some(record.clone())
    );
    drop(store);
    let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(retained(&reopened), before);
    assert_eq!(reopened.get(record.operation_id).unwrap(), Some(record));
}

#[test]
fn stored_json_materialization_v17_recording_agrees_with_default_decode() {
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 10_000);
    let record = finite_recording_record(&proposal);
    let bytes = encode(&record).unwrap();
    assert_eq!(
        crate::operation_journal::codec::decode_with_limits(&bytes, stored_json_limits()).unwrap(),
        decode(&bytes).unwrap()
    );
}
