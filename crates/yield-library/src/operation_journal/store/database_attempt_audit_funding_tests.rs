// Historical SQLite observations only; no receipt or allocation qualifies settlement.
#[test]
fn funded_outer_attempt_audit_retains_exact_collections_through_caller_destruction() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    for kind in [
        AttemptOutcomeKind::Unknown,
        AttemptOutcomeKind::NoCommitObservation,
        AttemptOutcomeKind::CommittedObservation,
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = persist_fixture(&store, &f.owner, 2);
        for index in 0..2 {
            let candidate = start(&record, &inventory, index);
            store.persist_database_attempt_start(&candidate).unwrap();
            store
                .persist_database_attempt_outcome(&outcome(&candidate, kind))
                .unwrap();
        }
        let before = retained_image(&store);
        let plain = store.audit_attempts_with_input(None).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        let pool = funding.pool_bytes_observer_for_test();
        let funded = store
            .audit_attempts_with_input(Some(&funding.input))
            .unwrap();
        assert!(funded.same_values(&plain));
        assert!(funding.remaining_bytes() < baseline);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(funded);
        assert_eq!(pool().0, pool().1);
        assert_eq!(retained_image(&store), before);
    }
}

#[test]
fn funded_outer_attempt_audit_keeps_partial_maps_and_selected_overlap_error_charged() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let first = start(&record, &inventory, 0);
    store.persist_database_attempt_start(&first).unwrap();
    // Unsupported retained evidence inserted directly; the actual writer refuses
    // this overlap. No genuine execution permission or settlement is fabricated.
    insert_raw_start(&store, &start(&record, &inventory, 0));
    let before = retained_image(&store);
    let plain = store
        .audit_attempts_with_input(None)
        .err()
        .unwrap()
        .to_string();
    assert!(plain.contains("overlapping unresolved physical"));
    let funding = JournalNamespaceFundingProbe::new();
    let pool = funding.pool_bytes_observer_for_test();
    let error = store
        .audit_attempts_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), plain);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(retained);
    assert_eq!(pool().0, pool().1);
    assert_eq!(retained_image(&store), before);
}

#[test]
fn funded_outer_attempt_audit_preserves_whole_page_utf8_before_uuid_validation() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 2);
    let first = start(&record, &inventory, 0);
    let second = start(&record, &inventory, 1);
    store.persist_database_attempt_start(&first).unwrap();
    store.persist_database_attempt_start(&second).unwrap();
    store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal_database_attempt SET attempt_id=?1 WHERE attempt_id=?2",
            params!["!".repeat(36), first.attempt.to_string()],
        )
        .unwrap();
    let mut invalid = vec![b'z'; 36];
    invalid[35] = 0xff;
    store.connection.execute("UPDATE operation_journal_database_attempt SET attempt_id=CAST(?1 AS TEXT) WHERE attempt_id=?2",params![invalid,second.attempt.to_string()]).unwrap();
    // Preserve deliberately invalid TEXT as raw bytes in the fixture snapshot.
    let image = || {
        store.connection.prepare("SELECT CAST(attempt_id AS BLOB),image FROM operation_journal_database_attempt ORDER BY attempt_id").unwrap()
            .query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?))).unwrap()
            .collect::<std::result::Result<Vec<_>,_>>().unwrap()
    };
    let before = image();
    let plain = store
        .audit_attempts_with_input(None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        plain.contains("UTF-8") || plain.contains("utf-8"),
        "{plain}"
    );
    let funding = JournalNamespaceFundingProbe::new();
    let error = store
        .audit_attempts_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), plain);
    drop(funding.retain_command_error_for_test(error));
    assert_eq!(image(), before);
}

#[test]
fn funded_outer_attempt_audit_refuses_pressure_before_owned_pages_and_preserves_native_errors() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let candidate = start(&record, &inventory, 0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let before = retained_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding
        .input
        .reserve_historical_scratch(funding.remaining_bytes() - 1)
        .unwrap();
    let error = store
        .audit_attempts_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert!(matches!(
        error,
        OperationJournalStoreError::HistoricalClaimResourceBound
    ));
    drop(funding.retain_command_error_for_test(error));
    drop(held);
    // Leave enough for actual outer working storage, then refuse page capacity.
    let held = funding
        .input
        .reserve_historical_scratch(
            funding.remaining_bytes() - database_attempts::audit_backing::working_bytes() - 1,
        )
        .unwrap();
    database_attempts::audit_backing::PAGE_CELLS.with(|n| n.set(0));
    let error = store
        .audit_attempts_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert!(matches!(
        error,
        OperationJournalStoreError::HistoricalClaimResourceBound
    ));
    assert_eq!(
        database_attempts::audit_backing::PAGE_CELLS.with(|n| n.get()),
        0
    );
    drop(funding.retain_command_error_for_test(error));
    drop(held);
    assert_eq!(retained_image(&store), before);
    store
        .connection
        .execute_batch(
            "ALTER TABLE operation_journal_database_attempt_outcome RENAME TO missing_outcome",
        )
        .unwrap();
    let plain = store
        .audit_attempts_with_input(None)
        .err()
        .unwrap()
        .to_string();
    let pool = funding.pool_bytes_observer_for_test();
    let error = store
        .audit_attempts_with_input(Some(&funding.input))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), plain);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(retained);
    assert_eq!(pool().0, pool().1);
}

#[test]
fn funded_outer_attempt_audit_collection_refusal_precedes_original_key_clone() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let candidate = start(&record, &inventory, 0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let before = retained_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let mut audit = database_attempts::Audit::default();
    let held = funding
        .input
        .reserve_historical_scratch(funding.remaining_bytes() - 1)
        .unwrap();
    database_attempts::audit_backing::PHYSICAL_COPIED_BYTES.with(|n| n.set(0));
    let error = audit
        .observe_start(&candidate, &inventory, Some(&funding.input))
        .unwrap_err();
    assert!(matches!(
        error,
        OperationJournalStoreError::HistoricalClaimResourceBound
    ));
    assert_eq!(
        database_attempts::audit_backing::PHYSICAL_COPIED_BYTES.with(|n| n.get()),
        0
    );
    assert!(audit.same_values(&database_attempts::Audit::default()));
    drop(held);
    audit
        .observe_start(&candidate, &inventory, Some(&funding.input))
        .unwrap();
    assert!(database_attempts::audit_backing::PHYSICAL_COPIED_BYTES.with(|n| n.get()) > 0);
    let pool = funding.pool_bytes_observer_for_test();
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(audit);
    assert_eq!(pool().0, pool().1);
    assert_eq!(retained_image(&store), before);
}
