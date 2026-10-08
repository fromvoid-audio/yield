// Actual readonly SQLite resource envelope, without native admission fixtures.
#[test]
fn recording_prestart_budget_keeps_complete_snapshot_and_original_rows() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    store.insert_intent(&record).unwrap();
    let before = store.recording_retained_totals().unwrap();
    store.begin_recording_prestart_read().unwrap();
    assert!(!store.connection.is_autocommit());
    assert_eq!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .unwrap(),
        proposal
    );
    let snapshot = store.rebuild_recording_prestart_snapshot().unwrap();
    assert!(snapshot.claims().is_empty());
    store.finish_recording_prestart_read().unwrap();
    assert!(store.connection.is_autocommit());
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert_eq!(store.get(record.operation_id).unwrap(), Some(record));
}

#[test]
fn recording_prestart_budget_refuses_oversized_before_original_blob_copy() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let record = finite_recording_record(
        &crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100),
    );
    store.insert_intent(&record).unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal SET record=zeroblob(65537) WHERE operation_id=?1",
            [record.operation_id.to_string()],
        )
        .unwrap();
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(0));
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| assert_eq!(bytes.get(), 0));
    let length: i64 = store
        .connection
        .query_row(
            "SELECT length(record) FROM operation_journal WHERE operation_id=?1",
            [record.operation_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(length, 65537);
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
}

#[test]
fn recording_prestart_budget_original_reader_refuses_before_blob_materialization() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal SET record=zeroblob(65537) WHERE operation_id=?1",
            [proposal.operation_id().to_string()],
        )
        .unwrap();
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(0));
    let result = store.recording_prestart_proposal(proposal.operation_id());
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| assert_eq!(bytes.get(), 0));
    assert!(matches!(
        result,
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    assert!(!store.recording_prestart_scope.get());
}

#[test]
fn recording_prestart_budget_refuses_foreign_capture_without_partial_validation() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    let before = store.recording_retained_totals().unwrap();
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    // General complete audit acceptance remains unchanged.
    store.rebuild_admission_snapshot().unwrap();
}

#[test]
fn recording_prestart_budget_refuses_complete_nonrecording_shape_and_preserves_generic_decode() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let record = intent_record(OperationId::new());
    store.insert_intent(&record).unwrap();
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    assert!(!store.recording_prestart_scope.get());
    assert_eq!(store.get(record.operation_id).unwrap(), Some(record));
    store.rebuild_admission_snapshot().unwrap();
}

#[test]
fn recording_prestart_budget_node_refusal_rolls_back_scope_and_preserves_exact_bytes() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let record = finite_recording_record(
        &crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100),
    );
    store.insert_intent(&record).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&encode(&record).unwrap()).unwrap();
    value["untrusted_unknown_array"] = serde_json::Value::Array(vec![serde_json::Value::Null; 600]);
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(bytes.len() < 65536);
    store
        .connection
        .execute(
            "UPDATE operation_journal SET record=?1 WHERE operation_id=?2",
            params![bytes, record.operation_id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    assert!(!store.recording_prestart_scope.get());
    let retained: Vec<u8> = store
        .connection
        .query_row(
            "SELECT record FROM operation_journal WHERE operation_id=?1",
            [record.operation_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, bytes);
    // Default strict parsing remains a semantic schema refusal, not our scoped quota.
    assert!(matches!(decode(&bytes), Err(DecodeError::Malformed { .. })));
}

#[test]
fn recording_prestart_budget_refuses_oversized_recovery_text_before_owned_row_reads() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let record = intent_record(OperationId::new());
    store.insert_intent(&record).unwrap();
    store
        .insert_active_recovery_charge_for_test(
            record.operation_id,
            &VolumeIdentity { device: 123 },
            4096,
        )
        .unwrap();
    let text = "x".repeat(65536);
    store
        .connection
        .execute(
            "UPDATE operation_journal_recovery_reserve SET purpose=?1",
            [&text],
        )
        .unwrap();
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    let retained: String = store
        .connection
        .query_row(
            "SELECT purpose FROM operation_journal_recovery_reserve",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, text);
}

#[test]
fn recording_prestart_budget_refuses_complete_count_before_decoding_any_record() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let records: Vec<_> = (0..33).map(|_| intent_record(OperationId::new())).collect();
    for record in &records {
        store.insert_intent(record).unwrap();
    }
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(0));
    assert!(matches!(
        store.begin_recording_prestart_read(),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    POINT_ROW_MATERIALIZED_BYTES.with(|bytes| assert_eq!(bytes.get(), 0));
    assert!(store.connection.is_autocommit());
    let rows: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 33);
    // Resource refusal does not impose a new general journal/global rebuild row ceiling.
    store.rebuild_admission_snapshot().unwrap();
    assert_eq!(
        store.get(records[0].operation_id).unwrap(),
        Some(records[0].clone())
    );
}

#[test]
fn recording_prestart_budget_refuses_hostile_control_before_generic_value_parser() {
    use crate::operation_journal::recording_commands::RECORDING_VALUE_PARSE_ENTRIES;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    // Under the complete raw envelope, but each singleton object owns a map node
    // in the generic parser before that parser's post-construction depth refusal.
    let hostile = format!("{}0{}", "{\"x\":".repeat(100), "}".repeat(100)).into_bytes();
    assert!(hostile.len() < 1024);
    store.connection.execute(
        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND slot=20000",
        rusqlite::params![hostile, proposal.operation_id().to_string()],
    ).unwrap();
    let retained = || {
        store.connection.query_row(
        "SELECT chunk_id,sequence,record FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20000",
        [proposal.operation_id().to_string()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
    ).unwrap()
    };
    let before = retained();
    assert_eq!(before.2, hostile);
    RECORDING_VALUE_PARSE_ENTRIES.with(|entries| entries.set(0));
    let result = store.recording_prestart_proposal(proposal.operation_id());
    RECORDING_VALUE_PARSE_ENTRIES.with(|entries| {
        assert_eq!(
            entries.get(),
            0,
            "scoped control refusal must precede generic Value parsing"
        )
    });
    assert!(matches!(
        result,
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.connection.is_autocommit());
    assert!(!store.recording_prestart_scope.get());
    assert_eq!(retained(), before);
    drop(store);
    let reopened = OperationJournalStore::open(&f.owner).unwrap();
    let after: (String, i64, Vec<u8>) = reopened.connection.query_row(
        "SELECT chunk_id,sequence,record FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20000",
        [proposal.operation_id().to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(after, before);
    assert!(reopened.connection.is_autocommit());
}

#[test]
fn recording_prestart_budget_noncanonical_control_keeps_generic_historical_acceptance() {
    use crate::operation_journal::recording_commands::RECORDING_VALUE_PARSE_ENTRIES;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let baseline = store.rebuild_admission_snapshot().unwrap();
    let before: (String, i64, Vec<u8>) = store.connection.query_row(
        "SELECT chunk_id,sequence,record FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20000",
        [proposal.operation_id().to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    let mut noncanonical = vec![b' '];
    noncanonical.extend_from_slice(&before.2);
    noncanonical.push(b'\n');
    assert!(noncanonical.len() <= 1024);
    store.connection.execute(
        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND slot=20000",
        rusqlite::params![noncanonical, proposal.operation_id().to_string()],
    ).unwrap();
    assert!(
        store
            .verify_recording_reservation(&proposal, false)
            .unwrap()
    );
    assert_eq!(store.rebuild_admission_snapshot().unwrap(), baseline);
    RECORDING_VALUE_PARSE_ENTRIES.with(|entries| entries.set(0));
    assert!(matches!(
        store.recording_prestart_proposal(proposal.operation_id()),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    RECORDING_VALUE_PARSE_ENTRIES.with(|entries| assert_eq!(entries.get(), 0));
    assert!(store.connection.is_autocommit());
    assert!(!store.recording_prestart_scope.get());
    let after: (String, i64, Vec<u8>) = store.connection.query_row(
        "SELECT chunk_id,sequence,record FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20000",
        [proposal.operation_id().to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!((after.0, after.1), (before.0, before.1));
    assert_eq!(after.2, noncanonical);
    assert!(
        store
            .verify_recording_reservation(&proposal, false)
            .unwrap()
    );
    assert_eq!(store.rebuild_admission_snapshot().unwrap(), baseline);
    drop(store);
    let reopened = OperationJournalStore::open(&f.owner).unwrap();
    assert!(
        reopened
            .verify_recording_reservation(&proposal, false)
            .unwrap()
    );
    assert_eq!(reopened.rebuild_admission_snapshot().unwrap(), baseline);
}

#[test]
fn recording_prestart_participant_excess_refuses_before_graph_and_preserves_row() {
    use crate::operation_journal::codec::PRESTART_VALUE_PARSE_ENTRIES;
    for (outer, field) in [
        (true, "descriptors"),
        (true, "checkpoints"),
        (false, "descriptors"),
        (false, "checkpoints"),
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal =
            crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
        let record = finite_recording_record(&proposal);
        store.insert_intent(&record).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode(&record).unwrap()).unwrap();
        let set = if outer {
            &mut value["universal_intent"]["participants"]
        } else {
            &mut value["universal_command_args"]["Recording"]["participants"]
        };
        set[field] = serde_json::Value::Array(vec![serde_json::json!({}); 65]);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(bytes.len() < 65536);
        store
            .connection
            .execute(
                "UPDATE operation_journal SET record=?1 WHERE operation_id=?2",
                params![bytes, record.operation_id.to_string()],
            )
            .unwrap();
        let retained = || {
            store.connection.query_row("SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1",[record.operation_id.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,Vec<u8>>(2)?))).unwrap()
        };
        let before = retained();
        PRESTART_VALUE_PARSE_ENTRIES.with(|entries| entries.set(0));
        let result = store.begin_recording_prestart_read();
        PRESTART_VALUE_PARSE_ENTRIES.with(|entries| {
            assert_eq!(
                entries.get(),
                0,
                "outer={outer} field={field}: entry65 must precede owned graph"
            )
        });
        assert!(matches!(
            result,
            Err(OperationJournalStoreError::RecordingPrestartResourceBound)
        ));
        assert!(store.connection.is_autocommit());
        assert!(!store.recording_prestart_scope.get());
        assert_eq!(retained(), before);
        assert!(matches!(decode(&bytes), Err(DecodeError::Malformed { .. })));
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        let after: (String, i64, Vec<u8>) = reopened
            .connection
            .query_row(
                "SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1",
                [record.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before);
        assert!(reopened.connection.is_autocommit());
    }
}

#[test]
fn recording_prestart_participant_preflight_preserves_original_fallback() {
    use crate::operation_journal::codec::PRESTART_VALUE_PARSE_ENTRIES;
    let p = crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&p);
    let mut value: serde_json::Value = serde_json::from_slice(&encode(&record).unwrap()).unwrap();
    value["universal_intent"]["participants"]["descriptors"] =
        serde_json::json!(vec![serde_json::Value::Null; 65]);
    let canonical = serde_json::to_string(&value).unwrap();
    for bytes in [
        canonical.replace("\"schema_version\":17", "\"schema_version\":18"),
        format!("{canonical} null"),
        canonical.replace("\"Recording\":", "\"Other\":null,\"Recording\":"),
    ] {
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
        let result = crate::operation_journal::codec::decode_prestart_recording(
            bytes.as_bytes(),
            recording_prestart_budget::JSON_LIMITS,
        );
        assert!(matches!(result, Ok(None) | Err(_)));
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| {
            assert_eq!(
                n.get(),
                1,
                "unsupported/noncomplete input retains original parser"
            )
        });
    }
    // Existing JSON quotas still govern ≤64 inventories, independently of semantic count.
    value["universal_intent"]["participants"]["descriptors"] =
        serde_json::json!(vec![serde_json::Value::Null; 64]);
    value["unknown_array"] = serde_json::json!(vec![serde_json::Value::Null; 600]);
    PRESTART_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
    assert!(matches!(
        crate::operation_journal::codec::decode_prestart_recording(
            &serde_json::to_vec(&value).unwrap(),
            recording_prestart_budget::JSON_LIMITS
        ),
        Err(crate::operation_journal::codec::BoundedDecodeError::LimitExceeded { .. })
    ));
    PRESTART_VALUE_PARSE_ENTRIES.with(|n| assert_eq!(n.get(), 1));
}

#[test]
fn recording_prestart_participant_preflight_duplicate_precedence_is_scoped() {
    use crate::operation_journal::codec::PRESTART_VALUE_PARSE_ENTRIES;
    let p = crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    let mut value: serde_json::Value =
        serde_json::from_slice(&encode(&finite_recording_record(&p)).unwrap()).unwrap();
    value["universal_command_args"]["Recording"]["participants"]["checkpoints"] =
        serde_json::json!(vec![serde_json::Value::Null; 65]);
    let wire = serde_json::to_string(&value).unwrap();
    for bytes in [
        wire.replace(
            "\"schema_version\":17",
            "\"schema_version\":17,\"schema_version\":17",
        ),
        wire.replace(
            "\"universal_command_args\":",
            "\"universal_command_args\":{\"Recording\":{}},\"universal_command_args\":",
        ),
        wire.replace("\"descriptors\":", "\"descriptors\":[],\"descriptors\":"),
        wire.replacen("{", "{\"unknown\":null,", 1),
    ] {
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
        assert!(matches!(
            crate::operation_journal::codec::decode_prestart_recording(
                bytes.as_bytes(),
                recording_prestart_budget::JSON_LIMITS
            ),
            Ok(None)
        ));
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| assert_eq!(n.get(), 0));
        assert!(matches!(
            decode(bytes.as_bytes()),
            Err(DecodeError::Malformed { .. })
        ));
    }
    // Repeated Recording keys make Args non-singleton, so the original parser wins.
    let bytes = wire.replace("\"Recording\":", "\"Recording\":{},\"Recording\":");
    PRESTART_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
    assert!(matches!(
        crate::operation_journal::codec::decode_prestart_recording(
            bytes.as_bytes(),
            recording_prestart_budget::JSON_LIMITS
        ),
        Err(crate::operation_journal::codec::BoundedDecodeError::Decode(
            DecodeError::Malformed { .. }
        ))
    ));
    PRESTART_VALUE_PARSE_ENTRIES.with(|n| assert_eq!(n.get(), 1));
}

#[test]
fn recording_prestart_participant_preflight_keeps_actual_retained_array_proposal() {
    use crate::operation_journal::codec::PRESTART_VALUE_PARSE_ENTRIES;
    for array in [false, true] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let p = crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
        let record = finite_recording_record(&p);
        store.insert_intent(&record).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode(&record).unwrap()).unwrap();
        if array {
            let proposal = &mut value["universal_command_args"]["Recording"];
            let set = &proposal["participants"];
            proposal["participants"] = serde_json::json!([set["descriptors"], set["checkpoints"]]);
            let fields = [
                "version",
                "operation_id",
                "output_id",
                "work_key",
                "participants",
                "destination_root",
                "staging_root",
                "destination",
                "staging_path",
                "format",
                "collision",
            ];
            let fields: Vec<_> = fields.iter().map(|key| proposal[*key].clone()).collect();
            *proposal = serde_json::Value::Array(fields);
        }
        let wire = format!(
            " \n{}\t",
            serde_json::to_string(&value)
                .unwrap()
                .replace("\"participants\"", "\"\\u0070articipants\"")
        );
        assert_eq!(decode(wire.as_bytes()).unwrap(), record);
        store
            .connection
            .execute(
                "UPDATE operation_journal SET record=?1 WHERE operation_id=?2",
                params![wire.as_bytes(), record.operation_id.to_string()],
            )
            .unwrap();
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
        store.begin_recording_prestart_read().unwrap();
        assert_eq!(
            store
                .recording_prestart_proposal(record.operation_id)
                .unwrap(),
            p
        );
        store.rebuild_recording_prestart_snapshot().unwrap();
        store.finish_recording_prestart_read().unwrap();
        PRESTART_VALUE_PARSE_ENTRIES.with(|n| {
            assert!(
                n.get() > 0,
                "accepted historical row must retain original parser"
            )
        });
        assert!(store.connection.is_autocommit());
        let before: (String, i64, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1",
                [record.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(before.2, wire.as_bytes());
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(reopened.get(record.operation_id).unwrap(), Some(record));
        let after: (String, i64, Vec<u8>) = reopened
            .connection
            .query_row(
                "SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1",
                [p.operation_id().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before);
        assert!(reopened.connection.is_autocommit());
    }
}

#[test]
fn recording_prestart_funded_inventory_reserves_peak_not_sum_before_decode() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let baseline = funding.remaining_bytes();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let working = store
        .begin_recording_prestart_read_with_input(Some(&funding.input))
        .unwrap()
        .unwrap();
    let first = baseline - funding.remaining_bytes();
    assert!(first < 3 * 1024 * 1024);
    assert_eq!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .unwrap(),
        proposal
    );
    store.finish_recording_prestart_read().unwrap();
    drop(working);
    assert_eq!(funding.remaining_bytes(), baseline);
    let second = crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&second))
        .unwrap();
    let working = store
        .begin_recording_prestart_read_with_input(Some(&funding.input))
        .unwrap()
        .unwrap();
    assert_eq!(baseline - funding.remaining_bytes(), first);
    assert_eq!(
        store
            .recording_prestart_proposal(second.operation_id())
            .unwrap(),
        second
    );
    store.finish_recording_prestart_read().unwrap();
    drop(working);
    assert_eq!(funding.remaining_bytes(), baseline);
}

#[test]
fn recording_prestart_funded_pressure_refuses_before_owned_record_and_ends_snapshot() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let pressure = funding
        .input
        .reserve_historical_scratch(funding.remaining_bytes())
        .unwrap();
    POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
    assert!(matches!(
        store.begin_recording_prestart_read_with_input(Some(&funding.input)),
        Err(OperationJournalStoreError::HistoricalClaimResourceBound)
    ));
    POINT_ROW_MATERIALIZED_BYTES.with(|n| assert_eq!(n.get(), 0));
    assert!(store.connection.is_autocommit());
    drop(pressure);
    assert_eq!(
        store.get(proposal.operation_id()).unwrap(),
        Some(finite_recording_record(&proposal))
    );
}

#[test]
fn recording_prestart_funded_decode_failure_retains_original_loan_through_consumer() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .connection
        .execute("UPDATE operation_journal SET record=?1", [b"{}".as_slice()])
        .unwrap();
    let error = store
        .begin_recording_prestart_read_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert!(store.connection.is_autocommit());
    let pool = funding.pool_bytes_observer_for_test();
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(error);
    assert_eq!(pool().0, pool().1);
}
