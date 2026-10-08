// Actual owned SQLite; original logical starts never issue live execution permission.
#[test]
fn funded_attempt_context_keeps_original_inventory_and_funds_historical_clone() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::database_attempts::record_backing::HISTORICAL_LABEL_CAPACITY;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let candidate = start(&record, &inventory, 0);
    let before = retained_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let baseline = funding.remaining_bytes();
    HISTORICAL_LABEL_CAPACITY.with(|n|n.set(0));
    let observed = store.attempt_inventory_with_input(&candidate, false, Some(&funding.input)).unwrap();
    assert_eq!(*observed, inventory);
    assert!(HISTORICAL_LABEL_CAPACITY.with(|n|n.get()) >= record.intent.label.len());
    assert!(funding.remaining_bytes() < baseline);
    drop(observed);
    assert_eq!(funding.remaining_bytes(), baseline);
    assert_eq!(retained_image(&store), before);
    let original = store.attempt_inventory_with_input(&candidate, false, None).unwrap();
    assert_eq!(*original, inventory);
}

#[test]
fn funded_attempt_context_preserves_historical_phase_and_fault_validation_order() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (stored, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let candidate = start(&stored, &inventory, 0);
    let before = retained_image(&store);
    for phase in [OperationPhase::IntentDurable, OperationPhase::SourceReconciled, OperationPhase::Terminal] {
        for disposition in [OperationDisposition::None, OperationDisposition::RetryPending, OperationDisposition::CancelRequestedBeforePublish] {
            for initial in [true, false] {
                for fault in 0..4 {
                    let mut actual = stored.clone();
                    actual.phase = phase;
                    actual.disposition = disposition;
                    if fault==1 {actual.intent.label.clear();}
                    let mut request = candidate.clone();
                    if fault==2 {request.database_bytes=u64::MAX;}
                    if fault==3 {request.nonce=Uuid::nil(); request.database_slice=Uuid::nil();}
                    let original = request.validate_bound(&actual, &inventory, initial).map_err(|error|
                        OperationJournalStoreError::InconsistentRecord {reason:format!("database claim inventory: attempt: {error}")}.to_string());
                    let funding = JournalNamespaceFundingProbe::new();
                    let retained = database_claims::read_with_input(&store.connection, stored.operation_id, Some(&funding.input)).unwrap().unwrap();
                    let result = crate::operation_journal::store::database_attempts::context_backing::validate(
                        &request, &actual, &retained, initial, &funding.input,
                    );
                    let result = match result {
                        Ok(())=>Ok(()),
                        Err(error)=>{
                            let text=error.to_string();
                            drop(funding.retain_command_error_for_test(error));
                            Err(text)
                        }
                    };
                    assert_eq!(result, original, "phase={phase:?} disposition={disposition:?} initial={initial} fault={fault}");
                }
            }
        }
    }
    assert_eq!(retained_image(&store), before);
}

#[test]
fn funded_attempt_context_refuses_pressure_before_clone_and_retains_selected_mismatch() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::database_attempts::record_backing::HISTORICAL_LABEL_CAPACITY;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let mut candidate = start(&record, &inventory, 0);
    let before = retained_image(&store);
    let funding = JournalNamespaceFundingProbe::new();
    let retained = database_claims::read_with_input(&store.connection, record.operation_id, Some(&funding.input)).unwrap().unwrap();
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes()-1).unwrap();
    HISTORICAL_LABEL_CAPACITY.with(|n|n.set(0));
    assert!(matches!(crate::operation_journal::store::database_attempts::context_backing::validate(
        &candidate, &record, &retained, false, &funding.input), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    assert_eq!(HISTORICAL_LABEL_CAPACITY.with(|n|n.get()), 0);
    drop(held);
    drop(retained);
    candidate.command_digest=[9;32];
    let original = store.attempt_inventory_with_input(&candidate, false, None).err().unwrap().to_string();
    let observed=funding.pool_bytes_observer_for_test();
    let error=store.attempt_inventory_with_input(&candidate, false, Some(&funding.input)).err().unwrap();
    assert_eq!(error.to_string(), original);
    let error=funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observed().0 < observed().1);
    drop(error);
    assert_eq!(observed().0, observed().1);
    assert_eq!(retained_image(&store), before);
}

#[test]
fn borrowed_historical_shape_matches_original_valid_overlay_encoding() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (mut record, _, inventory)=persist_fixture(&store,&f.owner,1);
    let original=crate::operation_journal::codec::encode(&record).unwrap();
    record.phase=OperationPhase::Terminal;
    record.disposition=OperationDisposition::CancelRequestedBeforePublish;
    let mut counted=Vec::new();
    crate::operation_journal::codec::serialize_full_record_inventory(
        &record, OperationPhase::IntentDurable, OperationDisposition::None, &mut counted,
    ).unwrap().unwrap();
    assert_eq!(counted,original);
    // The passive inventory serializer is never accepted in place of the actual
    // full codec validator; the current invalid V16 envelope still refuses.
    assert!(crate::operation_journal::codec::encode(&record).is_err());
    assert!(!inventory.bindings().is_empty());
}
