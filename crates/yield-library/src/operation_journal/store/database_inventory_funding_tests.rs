// Actual profile-owned SQLite; fixtures supply metadata observations only.
#[test]
fn funded_binary_inventory_retains_original_loans_until_decoded_inventory_dies() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let observe = funding.pool_bytes_observer_for_test();
    let before = funding.remaining_bytes();
    database_claims::INVENTORY_COPIED_BYTES.with(|n| n.set(0));
    let retained = database_claims::read_with_input(&store.connection, record.operation_id, Some(&funding.input)).unwrap().unwrap();
    assert_eq!(&*retained, &inventory);
    assert!(funding.remaining_bytes() < before);
    database_claims::INVENTORY_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    store.validate_database_claims_inside_transaction_with_input(Some(&funding.input)).unwrap();
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(retained);
    assert_eq!(observe().0, observe().1);
}

#[test]
fn funded_binary_inventory_pressure_refuses_without_mutating_original_claims() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes() - 1).unwrap();
    assert!(matches!(database_claims::read_with_input(&store.connection, record.operation_id, Some(&funding.input)), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    drop(held);
    assert_eq!(database_claims::read(&store.connection, record.operation_id).unwrap(), Some(inventory));
    assert_eq!(provider.claim_count(), 1);
    assert_eq!(rows(&store, ordinary_claims::TABLE_NAME), claims.claims().len() as i64);
}

#[test]
fn funded_binary_inventory_canonical_conflict_retains_selected_original_failure() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store.connection.execute("UPDATE operation_journal_database_binding SET main_file='changed' WHERE operation_id=?1 AND ordinal=0", [record.operation_id.to_string()]).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let observe = funding.pool_bytes_observer_for_test();
    let error = match database_claims::read_with_input(&store.connection, record.operation_id, Some(&funding.input)) { Err(error) => error, Ok(_) => panic!("corrupt original binding accepted") };
    assert!(matches!(&error, OperationJournalStoreError::InconsistentRecord { reason } if reason.contains("binding identity conflict")));
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(error);
    assert_eq!(observe().0, observe().1);
}

#[test]
fn funded_binary_inventory_missing_root_matches_original_fail_closed_reader() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store.connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store.connection.execute("DELETE FROM operation_journal_database_binding WHERE operation_id=?1 AND ordinal=0", [record.operation_id.to_string()]).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    assert!(database_claims::read(&store.connection, record.operation_id).is_err());
    assert!(database_claims::read_with_input(&store.connection, record.operation_id, Some(&funding.input)).is_err());
}
