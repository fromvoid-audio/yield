// Genuine SQLite rows and original semantic audits; synthetic claim observations
// below issue no live reservations, required permits or physical qualification.
fn persist_recording_database_fixture(
    store: &OperationJournalStore,
    owner: &WritableProfileGuard,
) -> (
    crate::operation_journal::recording_commands::RecordingProposal,
    StoredOperationRecord,
) {
    use crate::operation_journal::full_capacity::FullCapacityAllocation;
    let (proposal, mut record, _, inventory) = recording_database_inputs(owner);
    let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
        &store.database_root,
        std::path::Path::new(JOURNAL_DATABASE_FILE_NAME),
        &store.database_path,
    )
    .unwrap();
    let actual = crate::durable_capacity::descriptor_capacity_facts(&opened.file)
        .unwrap()
        .identity;
    let allocations = record
        .full_capacity_plan
        .as_ref()
        .unwrap()
        .allocations()
        .iter()
        .map(|a| {
            FullCapacityAllocation::new(
                if a.class() == FullCapacityAllocationClass::JournalRecord {
                    actual.clone()
                } else {
                    a.volume().clone()
                },
                a.allocation_unit(),
                a.class(),
                a.logical_bytes(),
                a.allocated_bytes(),
                a.protected_free_bytes(),
            )
            .unwrap()
        })
        .collect();
    record.full_capacity_plan = Some(FullCapacityPlan::new(allocations).unwrap());
    // Keep original slice claim IDs while updating only actual journal-volume context.
    let claims = AdmissionClaimInventory::new(
        record
            .full_capacity_plan
            .as_ref()
            .unwrap()
            .allocations()
            .iter()
            .map(|a| {
                let id = inventory
                    .slices()
                    .iter()
                    .find(|s| s.class == a.class())
                    .map(|s| AdmissionClaimId::from_uuid(s.claim).unwrap())
                    .unwrap_or_else(AdmissionClaimId::new);
                AdmissionClaim::new(id, a.volume().clone(), a.class(), a.allocated_bytes()).unwrap()
            })
            .collect(),
    )
    .unwrap();
    store.insert_intent(&record).unwrap();
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    ordinary_claims::insert_rows_inside_transaction(
        &store.connection,
        record.operation_id,
        &claims,
    )
    .unwrap();
    database_claims::insert(&store.connection, record.operation_id, &inventory).unwrap();
    store.connection.execute_batch("COMMIT").unwrap();
    (proposal, record)
}

#[test]
fn recording_prestart_database_claims_complete_original_audit_is_read_only() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (proposal, record) = persist_recording_database_fixture(&store, &f.owner);
    let before = integration::durable_image(&store);
    store.begin_recording_prestart_read().unwrap();
    assert_eq!(
        store
            .recording_prestart_proposal(record.operation_id)
            .unwrap(),
        proposal
    );
    let snapshot = store.rebuild_recording_prestart_snapshot().unwrap();
    assert_eq!(snapshot.claims().len(), 6);
    store.finish_recording_prestart_read().unwrap();
    assert_eq!(integration::durable_image(&store), before);
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store.begin_recording_prestart_read().unwrap();
    store.rebuild_recording_prestart_snapshot().unwrap();
    store.finish_recording_prestart_read().unwrap();
    assert_eq!(integration::durable_image(&store), before);
}
