#[test]
fn legacy_recovery_initial_claim_reader_requires_exact_immutable_unreleased_inventory() {
    // Actual SQL/accounting component fixture only. This constructs no Source recovery,
    // physical capacity qualification, resumed provisional lease or writable grant.
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let mut record = intent_record(OperationId::new());
    let claims = claims_for_intent(&record);
    let provider = provider_for_intent(&record);
    let plan = full_capacity_plan_for_record(&record).unwrap();
    let lease = provider.admit(&plan, &claims).unwrap();
    let settled = store.settle_initial_claims_with_lease(&record, &claims, &provider, lease).unwrap();
    assert!(matches!(settled.outcome(), JournalInsertOutcome::Committed(_)));
    let input = super::super::owner::InputPermit::historical_test_namespace_input(1);
    let snapshot = store.rebuild_admission_snapshot_for_input(&input).unwrap();
    // The helper validates only immutable rows/plan. Production callers additionally
    // require the actual funded complete V19 decoder and matching historical request.
    record.full_capacity_plan = Some(plan);
    let before = input.historical_test_remaining();
    store.validate_legacy_recovery_initial_claims(&record, snapshot.claims(), &input).unwrap();
    assert_eq!(input.historical_test_remaining(), before);
    let claim = &snapshot.claims()[0];
    for (release, audit, bytes, operation) in [
        (Some(Uuid::new_v4()), false, claim.allocated_bytes(), record.operation_id),
        (None, true, claim.allocated_bytes(), record.operation_id),
        (None, false, claim.allocated_bytes() + 1, record.operation_id),
        (None, false, claim.allocated_bytes(), OperationId::new()),
    ] {
        let changed = OrdinaryClaimRebuildSnapshot::for_test(vec![RebuildClaim::for_test(
            operation, claim.claim_id(), claim.volume().clone(), claim.allocation_class(),
            bytes, release, audit,
        )], CapacityRebuildReadiness::Ready);
        let input = super::super::owner::InputPermit::historical_test_namespace_input(1);
        assert!(store.validate_legacy_recovery_initial_claims(&record, changed.claims(), &input).is_err());
    }
    store.connection.execute("DELETE FROM operation_journal_ordinary_claim WHERE operation_id=?1", [record.operation_id.to_string()]).unwrap();
    assert!(store.validate_legacy_recovery_initial_claims(&record, snapshot.claims(), &input).is_err());
}

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
    let input = super::super::owner::InputPermit::historical_test_namespace_input(1);
    let available = input.historical_test_remaining();
    assert!(store.observe_claim_operation(operation_id, &input).unwrap());
    assert_eq!(input.historical_test_remaining(), available);
    let funded = store.rebuild_admission_snapshot_for_input(&input).unwrap();
    let old = store.rebuild_admission_snapshot().unwrap();
    assert!(funded.matches_original(&old));
    assert!(
        input.historical_test_remaining() < available,
        "snapshot retains its original collection backing"
    );
    drop(funded);
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

#[test]
fn historical_claim_sqlite_projection_gates_match_original_type_identity_and_sequence_refusals() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let operation = OperationId::new();
    let encoded = encode(&intent_record(operation)).unwrap();
    let original = String::from_utf8(encoded.clone()).unwrap();
    let mismatch = original
        .replace(&operation.to_string(), &OperationId::new().to_string())
        .into_bytes();
    let unknown = original
        .replace("\"schema_version\": 1", "\"schema_version\": 999")
        .into_bytes();
    assert_ne!(mismatch, encoded);
    assert_ne!(unknown, encoded);
    use rusqlite::types::Value as SqlValue;
    let cases = [
        (
            "valid",
            SqlValue::Integer(1),
            SqlValue::Blob(encoded.clone()),
            true,
        ),
        (
            "zero_sequence",
            SqlValue::Integer(0),
            SqlValue::Blob(encoded.clone()),
            false,
        ),
        (
            "negative_sequence",
            SqlValue::Integer(-1),
            SqlValue::Blob(encoded.clone()),
            false,
        ),
        (
            "text_sequence",
            SqlValue::Text("not-an-integer".into()),
            SqlValue::Blob(encoded.clone()),
            false,
        ),
        (
            "real_sequence",
            SqlValue::Real(1.5),
            SqlValue::Blob(encoded.clone()),
            false,
        ),
        (
            "text_record",
            SqlValue::Integer(1),
            SqlValue::Text(original.clone()),
            false,
        ),
        (
            "empty_blob",
            SqlValue::Integer(1),
            SqlValue::Blob(Vec::new()),
            false,
        ),
        (
            "wrong_identity",
            SqlValue::Integer(1),
            SqlValue::Blob(mismatch),
            false,
        ),
        (
            "unknown_schema",
            SqlValue::Integer(1),
            SqlValue::Blob(unknown),
            false,
        ),
        (
            "oversize_blob",
            SqlValue::Integer(1),
            SqlValue::Blob(vec![b' '; MAX_RECORD_BYTES + 1]),
            false,
        ),
    ];
    // Deliberately create retained corrupt rows without changing the schema or
    // admission code. The original and borrowed readers must both refuse them.
    store
        .connection
        .execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    let input = super::super::owner::InputPermit::historical_test_input(1);
    let available = input.historical_test_remaining();
    for (label, sequence, record, valid) in cases {
        store
            .connection
            .execute("DELETE FROM operation_journal", [])
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1,?2,?3)",
                params![operation.to_string(), sequence, record],
            )
            .unwrap();
        let before: (String, String, SqlValue) = store
            .connection
            .query_row(
                "SELECT typeof(sequence), typeof(record), record FROM operation_journal",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let old = store.get(operation);
        let new = store.observe_claim_operation(operation, &input);
        assert_eq!(
            old.as_ref().is_ok_and(Option::is_some),
            valid,
            "original {label}"
        );
        assert_eq!(
            new.as_ref().is_ok_and(|present| *present),
            valid,
            "borrowed {label}"
        );
        assert_eq!(old.is_err(), new.is_err(), "typed refusal {label}");
        assert_eq!(
            input.historical_test_remaining(),
            available,
            "credit {label}"
        );
        let after = store
            .connection
            .query_row(
                "SELECT typeof(sequence), typeof(record), record FROM operation_journal",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, SqlValue>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(before, after, "immutable retained row {label}");
    }
    store
        .connection
        .execute("DELETE FROM operation_journal", [])
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO operation_journal VALUES (?1,1,?2)",
            params![operation.to_string(), encoded],
        )
        .unwrap();
    // The original uniqueness check spans all existing observation families.
    store
        .connection
        .execute(
            "INSERT INTO operation_journal_full_progress VALUES (?1,1,?2)",
            params![OperationId::new().to_string(), b"{}".as_slice()],
        )
        .unwrap();
    assert!(store.get(operation).is_err());
    assert!(matches!(
        store.observe_claim_operation(operation, &input),
        Err(OperationJournalStoreError::RequiresAudit { .. })
    ));
    assert_eq!(input.historical_test_remaining(), available);
    store
        .connection
        .execute("DELETE FROM operation_journal_full_progress", [])
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO operation_journal_full_progress VALUES (?1,2,?2)",
            params![operation.to_string(), b"{}".as_slice()],
        )
        .unwrap();
    assert!(store.get(operation).is_err());
    assert!(matches!(
        store.observe_claim_operation(operation, &input),
        Err(OperationJournalStoreError::RequiresAudit { .. })
    ));
    assert_eq!(input.historical_test_remaining(), available);
}
