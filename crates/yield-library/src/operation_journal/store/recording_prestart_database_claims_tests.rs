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
    let before = recording_database_image(&store);
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
    assert_eq!(recording_database_image(&store), before);
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store.begin_recording_prestart_read().unwrap();
    store.rebuild_recording_prestart_snapshot().unwrap();
    store.finish_recording_prestart_read().unwrap();
    assert_eq!(recording_database_image(&store), before);
}

fn recording_database_image(store: &OperationJournalStore) -> Vec<String> {
    let mut image = integration::durable_image(store);
    for query in [
        "SELECT slice_id || ':' || operation_id || ':' || binding_id || ':' || claim_id || ':' || participant_id || ':' || direction || ':' || class || ':' || ceiling || ':' || wal_ceiling || ':' || shm_ceiling FROM operation_journal_database_claim_slice ORDER BY slice_id",
        "SELECT chunk_id || ':' || operation_id || ':' || slot || ':' || hex(record) FROM operation_journal_recording_chunks ORDER BY chunk_id",
    ] {
        image.extend(
            store
                .connection
                .prepare(query)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    image
}

#[test]
fn recording_prestart_database_claims_complete_multiple_inventory_is_not_truncated() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    for _ in 0..4 {
        persist_recording_database_fixture(&store, &f.owner);
    }
    assert_eq!(rows(&store, database_claims::SLICE_TABLE), 32);
    let before = recording_database_image(&store);
    store.begin_recording_prestart_read().unwrap();
    let snapshot = store.rebuild_recording_prestart_snapshot().unwrap();
    assert_eq!(snapshot.claims().len(), 24);
    store.finish_recording_prestart_read().unwrap();
    assert_eq!(recording_database_image(&store), before);
}

#[test]
fn recording_prestart_database_claims_shape_refuses_before_original_inventory_copy() {
    for corrupt in [
        "oversized",
        "lease_version",
        "wrong_type",
        "oversized_operation_id",
        "nul_operation_id",
        "nul_slice_id",
        "nul_claim_id",
        "nul_nonce",
        "wrong_id_type",
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        persist_recording_database_fixture(&store, &f.owner);
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON; PRAGMA foreign_keys=OFF")
            .unwrap();
        match corrupt {
            "oversized" => {
                store.connection.execute("UPDATE operation_journal_database_binding SET inventory=zeroblob(65537) WHERE ordinal=0", []).unwrap();
            }
            "lease_version" => {
                store.connection.execute("UPDATE operation_journal_database_binding SET inventory=CAST(X'595743420200' || substr(inventory,7) AS BLOB) WHERE ordinal=0", []).unwrap();
            }
            "oversized_operation_id" => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_database_binding SET operation_id=?1",
                        ["x".repeat(1024 * 1024)],
                    )
                    .unwrap();
            }
            "nul_operation_id" | "nul_slice_id" | "nul_claim_id" | "nul_nonce" => {
                let value = format!("{}\0{}", "a".repeat(36), "x".repeat(1024 * 1024));
                // The original character-count guard sees 36 despite the hidden suffix.
                let length: i64 = store
                    .connection
                    .query_row("SELECT length(?1)", [&value], |r| r.get(0))
                    .unwrap();
                assert_eq!(length, 36);
                let (table, field) = match corrupt {
                    "nul_operation_id" => (database_claims::BINDING_TABLE, "operation_id"),
                    "nul_slice_id" => (database_claims::SLICE_TABLE, "slice_id"),
                    "nul_claim_id" => (ordinary_claims::TABLE_NAME, "claim_id"),
                    _ => (database_claims::BINDING_TABLE, "decision_nonce"),
                };
                store.connection.execute(&format!("UPDATE {table} SET {field}=?1 WHERE rowid=(SELECT rowid FROM {table} LIMIT 1)"), [&value]).unwrap();
            }
            "wrong_id_type" => {
                store.connection.execute("UPDATE operation_journal_database_binding SET binding_id=zeroblob(36) WHERE ordinal=0", []).unwrap();
            }
            _ => {
                store.connection.execute("UPDATE operation_journal_database_binding SET inventory='retained unsupported text' WHERE ordinal=0", []).unwrap();
            }
        }
        let before = recording_database_image(&store);
        database_claims::INVENTORY_COPIED_BYTES.with(|n| n.set(0));
        assert!(
            matches!(
                store.begin_recording_prestart_read(),
                Err(OperationJournalStoreError::RecordingPrestartResourceBound)
            ),
            "{corrupt}"
        );
        database_claims::INVENTORY_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0, "{corrupt}"));
        assert!(store.connection.is_autocommit());
        assert_eq!(recording_database_image(&store), before);
    }
}

#[test]
fn recording_prestart_database_claims_semantic_refusal_preserves_all_evidence() {
    for corrupt in [
        "missing_slice",
        "changed_slice",
        "wrong_digest",
        "released_claim",
        "cross_operation",
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        persist_recording_database_fixture(&store, &f.owner);
        match corrupt {
            "missing_slice" => {
                store.connection.execute("DELETE FROM operation_journal_database_claim_slice WHERE slice_id=(SELECT slice_id FROM operation_journal_database_claim_slice LIMIT 1)", []).unwrap();
            }
            "changed_slice" => {
                store.connection.execute("UPDATE operation_journal_database_claim_slice SET slice_id=?1 WHERE slice_id=(SELECT slice_id FROM operation_journal_database_claim_slice LIMIT 1)", [Uuid::new_v4().to_string()]).unwrap();
            }
            "wrong_digest" => {
                store.connection.execute("UPDATE operation_journal_database_binding SET inventory_digest=zeroblob(32)", []).unwrap();
            }
            "released_claim" => {
                store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release(claim_id,operation_id,release_id) SELECT claim_id,operation_id,?1 FROM operation_journal_database_claim_slice LIMIT 1", [Uuid::new_v4().to_string()]).unwrap();
            }
            _ => {
                let proposal =
                    crate::operation_journal::recording_commands::recording_proposal_for_test(
                        2, 100,
                    );
                store
                    .insert_intent(&finite_recording_record(&proposal))
                    .unwrap();
                store
                    .connection
                    .execute_batch("PRAGMA foreign_keys=OFF")
                    .unwrap();
                store.connection.execute("UPDATE operation_journal_database_claim_slice SET operation_id=?1 WHERE slice_id=(SELECT slice_id FROM operation_journal_database_claim_slice LIMIT 1)", [proposal.operation_id().to_string()]).unwrap();
                store
                    .connection
                    .execute_batch("PRAGMA foreign_keys=ON")
                    .unwrap();
            }
        }
        let before = recording_database_image(&store);
        store.begin_recording_prestart_read().unwrap();
        assert!(
            store.rebuild_recording_prestart_snapshot().is_err(),
            "{corrupt}"
        );
        store.finish_recording_prestart_read().unwrap();
        assert_eq!(recording_database_image(&store), before, "{corrupt}");
    }
}
