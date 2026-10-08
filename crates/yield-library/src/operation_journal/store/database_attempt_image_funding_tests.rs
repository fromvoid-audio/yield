// Real SQLite logical observations only; no execution or settlement authority.
#[test]
fn funded_attempt_images_preserve_all_outcomes_and_retain_original_producer_loans() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use database_attempts::image_backing;
    for kind in [AttemptOutcomeKind::Unknown, AttemptOutcomeKind::NoCommitObservation, AttemptOutcomeKind::CommittedObservation] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
        let candidate = start(&record, &inventory, 0);
        store.persist_database_attempt_start(&candidate).unwrap();
        let observation = outcome(&candidate, kind);
        store.persist_database_attempt_outcome(&observation).unwrap();
        let before = retained_image(&store);
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        let observed_pool = funding.pool_bytes_observer_for_test();
        let retained = image_backing::read_start(&store.connection, candidate.attempt, Some(&funding.input)).unwrap().unwrap();
        assert_eq!(*retained, candidate);
        let retained_outcome = image_backing::read_outcome(&store.connection, &retained, Some(&funding.input)).unwrap().unwrap();
        assert_eq!(*retained_outcome, observation);
        assert!(funding.remaining_bytes() < baseline);
        // Integration exercises the actual funded audit call sites as well.
        store.validate_database_attempts_inside_transaction_with_input(Some(&funding.input)).unwrap();
        drop(funding);
        assert!(observed_pool().0 < observed_pool().1);
        drop(retained_outcome);
        assert!(observed_pool().0 < observed_pool().1);
        drop(retained);
        assert_eq!(observed_pool().0, observed_pool().1);
        assert_eq!(retained_image(&store), before);
    }
}

#[test]
fn funded_attempt_images_match_original_canonical_and_scalar_error_precedence() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use database_attempts::image_backing;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let candidate = start(&record, &inventory, 0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let original = candidate.encode().unwrap();
    let mut unknown: serde_json::Value = serde_json::from_slice(&original).unwrap();
    unknown.as_object_mut().unwrap().insert("unrecognized".into(), serde_json::Value::String("\\\"\u{1f642}".repeat(256)));
    for bytes in [b"{".to_vec(), [b" ".as_slice(), original.as_slice()].concat(), serde_json::to_vec(&unknown).unwrap(), b"null".to_vec()] {
        store.connection.execute("UPDATE operation_journal_database_attempt SET image=?1", [&bytes]).unwrap();
        let before = retained_image(&store);
        let plain = image_backing::read_start(&store.connection, candidate.attempt, None).err().unwrap().to_string();
        let funding = JournalNamespaceFundingProbe::new();
        let pool = funding.pool_bytes_observer_for_test();
        let error = image_backing::read_start(&store.connection, candidate.attempt, Some(&funding.input)).err().unwrap();
        assert_eq!(error.to_string(), plain, "image bytes={}", bytes.len());
        let retained_error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(retained_error);
        assert_eq!(pool().0, pool().1);
        assert_eq!(retained_image(&store), before);
    }
    store.connection.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    // Later numeric SQL conversion still precedes the malformed JSON decoder.
    store.connection.execute("UPDATE operation_journal_database_attempt SET image=?1,database_bytes=-1", [b"{".as_slice()]).unwrap();
    let plain = image_backing::read_start(&store.connection,candidate.attempt,None).err().unwrap().to_string();
    let funding = JournalNamespaceFundingProbe::new();
    let error = image_backing::read_start(&store.connection,candidate.attempt,Some(&funding.input)).err().unwrap();
    assert_eq!(error.to_string(),plain);
    drop(funding.retain_command_error_for_test(error));
}

#[test]
fn funded_attempt_images_refuse_before_typed_byte_ownership_and_preserve_absence() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use database_attempts::image_backing;
    use crate::operation_journal::database_attempts::inventory_bytes::OWNED_BYTES;
    let f=fixture();
    let store=OperationJournalStore::open(&f.owner).unwrap();
    let (record,_,inventory)=persist_fixture(&store,&f.owner,1);
    let candidate=start(&record,&inventory,0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let before=retained_image(&store);
    let funding=JournalNamespaceFundingProbe::new();
    let baseline=funding.remaining_bytes();
    assert!(image_backing::read_start(&store.connection,Uuid::new_v4(),Some(&funding.input)).unwrap().is_none());
    assert_eq!(funding.remaining_bytes(),baseline);
    let held=funding.input.reserve_historical_scratch(baseline-1).unwrap();
    OWNED_BYTES.with(|n|n.set(0));
    assert!(matches!(image_backing::read_start(&store.connection,candidate.attempt,Some(&funding.input)),Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    assert_eq!(OWNED_BYTES.with(|n|n.get()),0);
    drop(held);
    assert_eq!(retained_image(&store),before);
}

#[test]
fn funded_attempt_outcome_images_preserve_original_validation_and_native_errors() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use database_attempts::image_backing;
    let f=fixture();
    let store=OperationJournalStore::open(&f.owner).unwrap();
    let (record,_,inventory)=persist_fixture(&store,&f.owner,1);
    let candidate=start(&record,&inventory,0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let observation=outcome(&candidate,AttemptOutcomeKind::Unknown);
    store.persist_database_attempt_outcome(&observation).unwrap();
    let funding=JournalNamespaceFundingProbe::new();
    let start=image_backing::read_start(&store.connection,candidate.attempt,Some(&funding.input)).unwrap().unwrap();
    let plain_start=image_backing::read_start(&store.connection,candidate.attempt,None).unwrap().unwrap();
    let mut wrong=observation.clone(); wrong.start_digest=[7;32];
    for bytes in [b"{".to_vec(), [b" ".as_slice(), observation.encode().unwrap().as_slice()].concat(), wrong.encode().unwrap()] {
        store.connection.execute("UPDATE operation_journal_database_attempt_outcome SET image=?1",[bytes]).unwrap();
        let before=retained_image(&store);
        let plain=image_backing::read_outcome(&store.connection,&plain_start,None).err().unwrap().to_string();
        let error=image_backing::read_outcome(&store.connection,&start,Some(&funding.input)).err().unwrap();
        assert_eq!(error.to_string(),plain);
        drop(funding.retain_command_error_for_test(error));
        assert_eq!(retained_image(&store),before);
    }
    store.connection.execute_batch("ALTER TABLE operation_journal_database_attempt_outcome RENAME TO missing_outcome").unwrap();
    let plain=image_backing::read_outcome(&store.connection,&plain_start,None).err().unwrap().to_string();
    let error=image_backing::read_outcome(&store.connection,&start,Some(&funding.input)).err().unwrap();
    assert_eq!(error.to_string(),plain);
    drop(funding.retain_command_error_for_test(error));
}

#[test]
fn funded_attempt_image_inventory_overflow_and_binary_errors_keep_original_order() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use database_attempts::image_backing;
    let f=fixture();
    let store=OperationJournalStore::open(&f.owner).unwrap();
    let (record,_,inventory)=persist_fixture(&store,&f.owner,1);
    let candidate=start(&record,&inventory,0);
    store.persist_database_attempt_start(&candidate).unwrap();
    let mut oversized=candidate.clone();
    oversized.inventory=vec![0;crate::operation_journal::database_claims::MAX_INVENTORY_BYTES+2];
    let mut bad_binary=candidate.clone(); bad_binary.inventory=vec![0;16];
    let mut numeric=candidate.clone();
    numeric.command=serde_json::json!({"expanded":[1e20,-1e20,1e-6,f64::MAX],"text":"\\\"\u{1f642}"});
    let mut images=vec![serde_json::to_vec(&oversized).unwrap(),serde_json::to_vec(&bad_binary).unwrap(),serde_json::to_vec(&numeric).unwrap()];
    images.push(String::from_utf8(candidate.encode().unwrap()).unwrap().replace("\"inventory\"", "\"\\u0069nventory\"").into_bytes());
    for bytes in images {
        store.connection.execute("UPDATE operation_journal_database_attempt SET image=?1",[&bytes]).unwrap();
        let before=retained_image(&store);
        let plain=image_backing::read_start(&store.connection,candidate.attempt,None);
        let funding=JournalNamespaceFundingProbe::new();
        let observed=image_backing::read_start(&store.connection,candidate.attempt,Some(&funding.input));
        match (plain, observed) {
            (Ok(Some(plain)),Ok(Some(observed)))=>assert_eq!(*plain,*observed),
            (Err(plain),Err(observed))=>{
                assert_eq!(observed.to_string(),plain.to_string(),"image bytes={}",bytes.len());
                drop(funding.retain_command_error_for_test(observed));
            }
            _=>panic!("funded result differs for {} bytes",bytes.len()),
        }
        assert_eq!(retained_image(&store),before);
    }
}
