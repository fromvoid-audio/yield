#[test]
fn database_command_digest_funding_matches_original_and_retains_selected_digest_failure() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let funding = JournalNamespaceFundingProbe::new();
    let before = funding.remaining_bytes();
    database_claims::command_backing::validate_record(&record, &inventory, &funding.input).unwrap();
    assert_eq!(funding.remaining_bytes(), before);
    let mut bindings = inventory.bindings().to_vec();
    bindings[0].participants[0].command_digest = [9;32];
    let bad = DatabaseClaimInventory::new(inventory.header().clone(), bindings, inventory.slices().to_vec()).unwrap();
    let original = bad.validate_record(&record).unwrap_err().to_string();
    let observed = funding.pool_bytes_observer_for_test();
    let error = database_claims::command_backing::validate_record(&record, &bad, &funding.input).unwrap_err();
    assert!(error.to_string().contains(&original));
    assert!(original.contains("immutable command digest mismatch"));
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observed().0 < observed().1);
    drop(error);
    assert_eq!(observed().0, observed().1);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn database_command_digest_funding_refuses_output_pressure_and_preserves_codec_bytes() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, claims, inventory) = input(&f.owner, 2);
    let provider = logical_provider(&record);
    let _settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
    let encoded = crate::operation_journal::codec::encode(&record).unwrap();
    let mut counted = Vec::new();
    crate::operation_journal::codec::serialize_record(&record, &mut counted).unwrap().unwrap();
    assert_eq!(counted, encoded);
    let digest = command_digest(&record, 1).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    // Working validation fits, but the original owned canonical graph does not.
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes()-8192).unwrap();
    assert!(matches!(database_claims::command_backing::validate_record(&record, &inventory, &funding.input), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    drop(held);
    inventory.validate_record(&record).unwrap();
    assert_eq!(command_digest(&record, 1).unwrap(), digest);
    assert_eq!(crate::operation_journal::codec::encode(&record).unwrap(), encoded);
    assert_eq!(rows(&store, ordinary_claims::TABLE_NAME), claims.claims().len() as i64);
    assert_eq!(provider.claim_count(), 1);
}
