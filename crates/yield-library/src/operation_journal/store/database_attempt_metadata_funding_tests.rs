// Real profile-owned metadata checks, with no fabricated physical admission witness.
#[test]
fn funded_attempt_metadata_matches_original_and_retains_selected_claim_failure() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = integration::actual_journal_input(&store, &f.owner);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let before = funding.remaining_bytes();
    store.validate_database_attempt_journal_claims(&record).unwrap();
    store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)).unwrap();
    assert_eq!(funding.remaining_bytes(), before);
    store.connection.execute("UPDATE operation_journal_ordinary_claim SET allocated_bytes=allocated_bytes+1 WHERE allocation_class='JournalRecord'", []).unwrap();
    let original = store.validate_database_attempt_journal_claims(&record).unwrap_err().to_string();
    let observed = funding.pool_bytes_observer_for_test();
    let error = store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original);
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observed().0 < observed().1);
    drop(error);
    assert_eq!(observed().0, observed().1);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn funded_attempt_metadata_preserves_whole_page_utf8_before_cardinality_and_uuid() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = integration::actual_journal_input(&store, &f.owner);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    store.connection.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON").unwrap();
    store.connection.execute("UPDATE operation_journal_ordinary_claim SET claim_id=?1 WHERE allocation_class='JournalRecord'", ["!".repeat(36)]).unwrap();
    let mut invalid_utf8 = vec![b'z';36];
    invalid_utf8[35] = 255;
    store.connection.execute("INSERT INTO operation_journal_ordinary_claim VALUES(CAST(?1 AS TEXT),?2,'1','JournalRecord',1)", params![invalid_utf8,record.operation_id.to_string()]).unwrap();
    let original = store.validate_database_attempt_journal_claims(&record).unwrap_err();
    assert!(matches!(&original, OperationJournalStoreError::Sql(rusqlite::Error::FromSqlConversionFailure(..))));
    let funding = JournalNamespaceFundingProbe::new();
    let error = store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original.to_string());
    assert!(matches!(&error, OperationJournalStoreError::Sql(rusqlite::Error::FromSqlConversionFailure(..))));
    drop(funding.retain_command_error_for_test(error));
}

#[test]
fn funded_attempt_metadata_refuses_pressure_before_descriptor_and_keeps_rows() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = integration::actual_journal_input(&store, &f.owner);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let before = integration::durable_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes()-1).unwrap();
    assert!(matches!(store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    drop(held);
    store.validate_database_attempt_journal_claims(&record).unwrap();
    assert_eq!(integration::durable_image(&store), before);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn funded_attempt_metadata_preserves_canonical_key_release_and_projection_bounds() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    for kind in 0..3 {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = integration::actual_journal_input(&store, &f.owner);
        let provider = logical_provider(&record);
        let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        let claim = claims.claims().iter().find(|c|c.allocation_class()==FullCapacityAllocationClass::JournalRecord).unwrap();
        store.connection.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON").unwrap();
        match kind {
            0 => { store.connection.execute("UPDATE operation_journal_ordinary_claim SET claim_id='ABCDEFAB-CDEF-ABCD-EFAB-CDEFABCDEFAB' WHERE allocation_class='JournalRecord'", []).unwrap(); }
            1 => { store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release VALUES(?1,?2,?3)", params![claim.claim_id().to_string(),record.operation_id.to_string(),Uuid::new_v4().to_string()]).unwrap(); }
            _ => for _ in 0..257 { store.connection.execute("INSERT INTO operation_journal_ordinary_claim VALUES(?1,?2,'1','JournalRecord',1)", params![Uuid::new_v4().to_string(),record.operation_id.to_string()]).unwrap(); },
        }
        let original = store.validate_database_attempt_journal_claims(&record).unwrap_err().to_string();
        let funding = JournalNamespaceFundingProbe::new();
        let error = store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)).unwrap_err();
        assert_eq!(error.to_string(), original);
        assert!(original.contains(match kind {0=>"key identity",1=>"differs from retained original plan",_=>"cardinality"}));
        drop(funding.retain_command_error_for_test(error));
        assert_eq!(provider.claim_count(), 1);
    }
}

#[test]
fn funded_attempt_metadata_retains_actual_descriptor_and_native_sql_failures() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    #[cfg(unix)]
    let cases = [true, false];
    #[cfg(not(unix))]
    let cases = [false];
    for descriptor in cases {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = integration::actual_journal_input(&store, &f.owner);
        let provider = logical_provider(&record);
        let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        if descriptor {
            #[cfg(unix)]
            std::fs::rename(&store.database_path, store.database_path.with_extension("retained-original")).unwrap();
        } else {
            store.connection.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE operation_journal_ordinary_claim").unwrap();
        }
        let original = store.validate_database_attempt_journal_claims(&record).unwrap_err().to_string();
        let funding = JournalNamespaceFundingProbe::new();
        let observed = funding.pool_bytes_observer_for_test();
        let error = store.validate_database_attempt_journal_claims_with_input(&record, Some(&funding.input)).unwrap_err();
        assert_eq!(error.to_string(), original);
        let error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(observed().0 < observed().1);
        drop(error);
        assert_eq!(observed().0, observed().1);
    }
}
