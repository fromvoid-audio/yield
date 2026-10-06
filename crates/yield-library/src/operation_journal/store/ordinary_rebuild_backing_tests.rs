#[test]
fn ordinary_rebuild_backing_matches_zero_small_and_complete_multiple_pages() {
    for count in [0, 1, ordinary_claims::MAX_PAGE_ROWS + 3] {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        store.insert_intent(&intent_record(operation)).unwrap();
        for index in 1..=count {
            insert_ordinary_claim(
                &store,
                Uuid::from_u128(index as u128),
                operation.as_uuid(),
                "7",
                "DestinationStaging",
                index as i64,
            )
            .unwrap();
        }
        for released in [false, true] {
            if released {
                store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release SELECT claim_id,operation_id,claim_id FROM operation_journal_ordinary_claim", []).unwrap();
            }
            let old = store.rebuild_admission_snapshot().unwrap();
            let input = super::super::owner::InputPermit::historical_test_input(1);
            let available = input.historical_test_remaining();
            let funded = store.rebuild_admission_snapshot_for_input(&input).unwrap();
            assert_eq!(*funded, old);
            assert_eq!(funded.claims.len(), count);
            assert!(input.historical_test_remaining() < available);
            drop(funded);
            assert_eq!(input.historical_test_remaining(), available);
        }
    }
}

#[test]
fn ordinary_rebuild_backing_preserves_malformed_missing_and_orphan_audit() {
    for scenario in ["missing", "malformed", "orphan", "mismatch"] {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let claim = Uuid::new_v4();
        if scenario != "missing" {
            store.insert_intent(&intent_record(operation)).unwrap();
        }
        insert_ordinary_claim(
            &store,
            claim,
            operation.as_uuid(),
            "7",
            "DestinationStaging",
            1,
        )
        .unwrap();
        if scenario == "malformed" {
            store.connection.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE operation_journal_ordinary_claim SET allocation_class='unknown'; PRAGMA ignore_check_constraints=OFF").unwrap();
        }
        if scenario == "orphan" || scenario == "mismatch" {
            // Exact existing corruption-fixture mechanism: original table/reader,
            // with a retained impossible relation rather than a schema substitute.
            store
                .connection
                .execute_batch("PRAGMA foreign_keys=OFF")
                .unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO operation_journal_ordinary_claim_release VALUES (?1,?2,?3)",
                    params![
                        if scenario == "orphan" {
                            Uuid::new_v4()
                        } else {
                            claim
                        }
                        .to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string()
                    ],
                )
                .unwrap();
            store
                .connection
                .execute_batch("PRAGMA foreign_keys=ON")
                .unwrap();
        }
        let old = store.rebuild_admission_snapshot().unwrap();
        let input = super::super::owner::InputPermit::historical_test_input(1);
        let available = input.historical_test_remaining();
        let funded = store.rebuild_admission_snapshot_for_input(&input).unwrap();
        assert_eq!(*funded, old, "{scenario}");
        drop(funded);
        assert_eq!(input.historical_test_remaining(), available, "{scenario}");
    }
}

#[test]
fn ordinary_rebuild_backing_refuses_collection_pressure_without_partial_snapshot() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let operation = OperationId::new();
    store.insert_intent(&intent_record(operation)).unwrap();
    insert_ordinary_claim(
        &store,
        Uuid::new_v4(),
        operation.as_uuid(),
        "7",
        "DestinationStaging",
        1,
    )
    .unwrap();
    let before = store.rebuild_admission_snapshot().unwrap();
    let limit = super::super::owner::InputPermit::historical_test_ordinary_limit();
    let input = super::super::owner::InputPermit::historical_test_input(limit - 4096);
    let available = input.historical_test_remaining();
    assert!(matches!(
        store.rebuild_admission_snapshot_for_input(&input),
        Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert_eq!(input.historical_test_remaining(), available);
    assert_eq!(store.rebuild_admission_snapshot().unwrap(), before);
    assert!(store.connection.is_autocommit());
}
