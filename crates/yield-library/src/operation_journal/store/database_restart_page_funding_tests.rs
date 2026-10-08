// Real owned SQLite observations. Logical fixture leases are not native qualification.
#[test]
fn funded_database_restart_pages_keep_original_inventory_and_refund_complete_scan() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let baseline = funding.remaining_bytes();
    store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap();
    assert_eq!(funding.remaining_bytes(), baseline);
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let before = integration::durable_image(&store);
    store
        .validate_database_claims_inside_transaction_with_input(None)
        .unwrap();
    super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
    database_claims::INVENTORY_COPIED_BYTES.with(|n| n.set(0));
    database_claims::restart_backing::PAGE_CELLS.with(|n| n.set(0));
    store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap();
    assert_eq!(
        super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.get()),
        0
    );
    assert_eq!(database_claims::INVENTORY_COPIED_BYTES.with(|n| n.get()), 0);
    assert!(database_claims::restart_backing::PAGE_CELLS.with(|n| n.get()) >= 2);
    assert_eq!(funding.remaining_bytes(), baseline);
    assert_eq!(integration::durable_image(&store), before);
    assert_eq!(
        database_claims::read(&store.connection, record.operation_id).unwrap(),
        Some(inventory)
    );
}

#[test]
fn funded_database_restart_pages_preserve_whole_page_utf8_before_uuid_errors() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal_database_binding SET operation_id=?1 WHERE ordinal=0",
            ["!".repeat(36)],
        )
        .unwrap();
    let mut invalid = vec![b'z'; 36];
    invalid[35] = 0xff;
    store.connection.execute("UPDATE operation_journal_database_binding SET operation_id=CAST(?1 AS TEXT) WHERE ordinal=1",[invalid]).unwrap();
    let image = || {
        store.connection.prepare("SELECT CAST(operation_id AS BLOB),inventory FROM operation_journal_database_binding ORDER BY ordinal").unwrap()
        .query_map([],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Option<Vec<u8>>>(1)?))).unwrap()
        .collect::<std::result::Result<Vec<_>,_>>().unwrap()
    };
    let before = image();
    let plain = store
        .validate_database_claims_inside_transaction_with_input(None)
        .unwrap_err()
        .to_string();
    assert!(
        plain.contains("UTF-8") || plain.contains("utf-8"),
        "{plain}"
    );
    let funding = JournalNamespaceFundingProbe::new();
    let pool = funding.pool_bytes_observer_for_test();
    let error = store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap_err();
    assert_eq!(error.to_string(), plain);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(retained);
    assert_eq!(pool().0, pool().1);
    assert_eq!(image(), before);
}

#[test]
fn funded_database_restart_pages_refuse_pressure_before_ownership_and_retain_native_errors() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let before = integration::durable_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding
        .input
        .reserve_historical_scratch(
            funding.remaining_bytes() - database_claims::restart_backing::working_bytes() - 1,
        )
        .unwrap();
    database_claims::restart_backing::PAGE_CELLS.with(|n| n.set(0));
    let error = store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap_err();
    assert!(matches!(
        error,
        OperationJournalStoreError::HistoricalClaimResourceBound
    ));
    assert_eq!(
        database_claims::restart_backing::PAGE_CELLS.with(|n| n.get()),
        0
    );
    drop(funding.retain_command_error_for_test(error));
    drop(held);
    assert_eq!(integration::durable_image(&store), before);
    store
        .connection
        .execute_batch("ALTER TABLE operation_journal_database_claim_slice RENAME TO missing_slice")
        .unwrap();
    let plain = store
        .validate_database_claims_inside_transaction_with_input(None)
        .unwrap_err()
        .to_string();
    let pool = funding.pool_bytes_observer_for_test();
    let error = store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap_err();
    assert_eq!(error.to_string(), plain);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(retained);
    assert_eq!(pool().0, pool().1);
}

#[test]
fn funded_database_restart_pages_preserve_selected_original_nested_inventory_failure() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store
        .connection
        .execute(
            "UPDATE operation_journal_database_binding SET main_file='changed' WHERE ordinal=0",
            [],
        )
        .unwrap();
    let before = integration::durable_image(&store);
    let expected = JournalNamespaceFundingProbe::new();
    // Compare to the already-funded original inner producer, whose historical
    // error label differs from the compatibility reader's label.
    let original = database_claims::read_with_input(
        &store.connection,
        record.operation_id,
        Some(&expected.input),
    )
    .err()
    .unwrap();
    let plain = original.to_string();
    drop(expected.retain_command_error_for_test(original));
    drop(expected);
    let funding = JournalNamespaceFundingProbe::new();
    let pool = funding.pool_bytes_observer_for_test();
    let error = store
        .validate_database_claims_inside_transaction_with_input(Some(&funding.input))
        .unwrap_err();
    assert_eq!(error.to_string(), plain);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(retained);
    assert_eq!(pool().0, pool().1);
    assert_eq!(integration::durable_image(&store), before);
}
