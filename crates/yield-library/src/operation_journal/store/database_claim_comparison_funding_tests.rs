// Actual original inventory/claim SQLite; fixtures provide observations only.
#[test]
fn funded_database_claim_comparison_matches_original_and_retains_selected_coverage_failure() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let before = funding.remaining_bytes();
    store.validate_database_claim_operation(record.operation_id, &inventory).unwrap();
    store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap();
    assert_eq!(funding.remaining_bytes(), before);
    store.connection.execute("UPDATE operation_journal_ordinary_claim SET allocated_bytes=allocated_bytes+1 WHERE operation_id=?1 AND allocation_class='SourceDatabaseCommit'", [record.operation_id.to_string()]).unwrap();
    let original = store.validate_database_claim_operation(record.operation_id, &inventory).unwrap_err().to_string();
    let observe = funding.pool_bytes_observer_for_test();
    let error = store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original);
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(error);
    assert_eq!(observe().0, observe().1);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn funded_database_claim_comparison_preserves_whole_page_utf8_before_uuid_error() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store.connection.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON").unwrap();
    // Both projected TEXT cells have SQLite length36. The earlier malformed UUID
    // must not mask the later pinned-driver UTF-8 conversion failure.
    let invalid_uuid = "!".repeat(36);
    store.connection.execute("UPDATE operation_journal_ordinary_claim SET claim_id=?1 WHERE rowid=(SELECT rowid FROM operation_journal_ordinary_claim WHERE allocation_class='SourceDatabaseCommit' LIMIT 1)", [&invalid_uuid]).unwrap();
    let mut invalid_utf8 = vec![b'z'; 36];
    invalid_utf8[35] = 255;
    store.connection.execute("UPDATE operation_journal_ordinary_claim SET claim_id=CAST(?1 AS TEXT) WHERE rowid=(SELECT rowid FROM operation_journal_ordinary_claim WHERE allocation_class='SourceWalShm' LIMIT 1)", [&invalid_utf8]).unwrap();
    let original = store.validate_database_claim_operation(record.operation_id, &inventory).unwrap_err();
    assert!(matches!(&original, OperationJournalStoreError::Sql(rusqlite::Error::FromSqlConversionFailure(..))));
    let funding = JournalNamespaceFundingProbe::new();
    let error = store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original.to_string());
    assert!(matches!(&error, OperationJournalStoreError::Sql(rusqlite::Error::FromSqlConversionFailure(..))));
    drop(funding.retain_command_error_for_test(error));
}

#[test]
fn funded_database_claim_comparison_preserves_original_release_and_budget_refusal() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes()-1).unwrap();
    assert!(matches!(store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    drop(held);
    store.validate_database_claim_operation(record.operation_id, &inventory).unwrap();
    // Immutable historical release observation, never genuine settlement authority.
    store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release VALUES(?1,?2,?3)", params![inventory.slices()[0].claim.to_string(), record.operation_id.to_string(), Uuid::new_v4().to_string()]).unwrap();
    let original = store.validate_database_claim_operation(record.operation_id, &inventory).unwrap_err().to_string();
    let error = store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original);
    assert!(original.contains("released without attempt settlement"));
    drop(funding.retain_command_error_for_test(error));
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn funded_database_claim_comparison_preserves_original_excess_projection_bound() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    for _ in 0..257 {
        store.connection.execute("INSERT INTO operation_journal_ordinary_claim VALUES(?1,?2,'1','SourceDatabaseCommit',1)", params![Uuid::new_v4().to_string(), record.operation_id.to_string()]).unwrap();
    }
    let original = store.validate_database_claim_operation(record.operation_id, &inventory).unwrap_err().to_string();
    let funding = JournalNamespaceFundingProbe::new();
    let error = store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original);
    assert!(original.contains("operation database claim bound"));
    drop(funding.retain_command_error_for_test(error));
    assert_eq!(rows(&store, ordinary_claims::TABLE_NAME), 257 + claims.claims().len() as i64);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn funded_database_claim_comparison_retains_original_native_sql_error_and_record_loans() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store.connection.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE operation_journal_ordinary_claim").unwrap();
    let original = store.validate_database_claim_operation(record.operation_id, &inventory).unwrap_err().to_string();
    let funding = JournalNamespaceFundingProbe::new();
    let observe = funding.pool_bytes_observer_for_test();
    let error = store.validate_database_claim_operation_with_input(record.operation_id, &inventory, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original);
    assert!(matches!(&error, OperationJournalStoreError::Sql(..)));
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(error);
    assert_eq!(observe().0, observe().1);
}
