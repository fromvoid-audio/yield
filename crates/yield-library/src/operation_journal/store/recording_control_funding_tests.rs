// Actual original profile/Store; control observations never supply terminal authority.
#[test]
fn funded_recording_controls_preserve_canonical_and_historical_acceptance_without_blob_copy() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    store.insert_intent(&record).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let available = funding.remaining_bytes();
    super::recording_chunks::PAYLOAD_COPIED_BYTES.with(|n| n.set(0));
    assert!(store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)).unwrap());
    super::recording_chunks::PAYLOAD_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    assert_eq!(funding.remaining_bytes(), available);
    store.recording_prestart_scope.set(true);
    assert!(store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)).unwrap());
    store.recording_prestart_scope.set(false);
    let original: Vec<u8> = store.connection.query_row("SELECT record FROM operation_journal_recording_chunks WHERE slot=20000", [], |r| r.get(0)).unwrap();
    let mut noncanonical = b" \n".to_vec();
    noncanonical.extend_from_slice(&original);
    store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20000", [&noncanonical]).unwrap();
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    assert!(store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)).unwrap());
    store.recording_prestart_scope.set(true);
    let error = match store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)) { Err(error) => error, Ok(_) => panic!("prestart accepted noncanonical original control") };
    store.recording_prestart_scope.set(false);
    assert!(matches!(error, OperationJournalStoreError::RecordingPrestartResourceBound));
    drop(funding.retain_command_error_for_test(error));
    let unchanged: Vec<u8> = store.connection.query_row("SELECT record FROM operation_journal_recording_chunks WHERE slot=20000", [], |r| r.get(0)).unwrap();
    assert_eq!(unchanged, noncanonical);
}

#[test]
fn funded_recording_control_refusal_and_selected_failure_keep_original_evidence() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store.insert_intent(&finite_recording_record(&proposal)).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let held = funding.input.reserve_historical_scratch(funding.remaining_bytes()-1).unwrap();
    assert!(matches!(store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)), Err(OperationJournalStoreError::HistoricalClaimResourceBound)));
    drop(held);
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20000", [b"{}".as_slice()]).unwrap();
    let observe = funding.pool_bytes_observer_for_test();
    let original_error = store.verify_recording_reservation(&proposal, true).unwrap_err().to_string();
    let error = store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)).unwrap_err();
    assert_eq!(error.to_string(), original_error);
    let retained = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(retained);
    assert_eq!(observe().0, observe().1);
    assert_eq!(store.recording_retained_totals().unwrap().0, 4);
}

#[test]
fn funded_recording_inventory_borrows_complete_original_proposal_and_preserves_error_loans() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    store.insert_intent(&record).unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let available = funding.remaining_bytes();
    super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
    store.validate_recording_inventory_inside_transaction_with_input(Some(&funding.input)).unwrap();
    super::POINT_ROW_MATERIALIZED_BYTES.with(|n| assert_eq!(n.get(), 0));
    assert_eq!(funding.remaining_bytes(), available);
    store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20000", [b"{}".as_slice()]).unwrap();
    let observe = funding.pool_bytes_observer_for_test();
    let error = store.validate_recording_inventory_inside_transaction_with_input(Some(&funding.input)).unwrap_err();
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(observe().0 < observe().1);
    drop(error);
    assert_eq!(observe().0, observe().1);
}

#[test]
fn funded_recording_control_second_payload_projection_rejects_changed_original_bound_without_copy() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    use super::recording_chunks::{PAYLOAD_READ_HOOK, PAYLOAD_COPIED_BYTES, PayloadReader};
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store.insert_intent(&finite_recording_record(&proposal)).unwrap();
    let changed = vec![b'x'; 4097];
    let for_hook = changed.clone();
    PAYLOAD_READ_HOOK.with(|hook| *hook.borrow_mut() = Some((PayloadReader::Control, Box::new(move |connection| {
        assert!(connection.is_autocommit());
        connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20000", [&for_hook]).unwrap();
    }))));
    PAYLOAD_COPIED_BYTES.with(|n| n.set(0));
    let funding = JournalNamespaceFundingProbe::new();
    let error = store.verify_recording_reservation_with_input(&proposal, true, Some(&funding.input)).unwrap_err();
    assert!(matches!(&error, OperationJournalStoreError::RecordingRetention { reason } if reason.contains("recording control exceeds reserved bound")));
    PAYLOAD_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    let unchanged: Vec<u8> = store.connection.query_row("SELECT record FROM operation_journal_recording_chunks WHERE slot=20000", [], |row| row.get(0)).unwrap();
    assert_eq!(unchanged, changed);
    drop(funding.retain_command_error_for_test(error));
}
