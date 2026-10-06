// Real SQLite corruption readback; synthetic observations issue no live permits.
fn scalar_database_image(store: &OperationJournalStore) -> Vec<Vec<Vec<u8>>> {
    use rusqlite::types::ValueRef;
    let mut image = Vec::new();
    for table in [
        "operation_journal_database_binding",
        "operation_journal_database_claim_slice",
        "operation_journal_ordinary_claim",
    ] {
        let mut query = store
            .connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap();
        let count = query.column_count();
        image.extend(
            query
                .query_map([], |row| {
                    (0..count)
                        .map(|i| {
                            Ok(match row.get_ref(i)? {
                                ValueRef::Null => vec![0],
                                ValueRef::Integer(v) => {
                                    [vec![1], v.to_le_bytes().to_vec()].concat()
                                }
                                ValueRef::Real(v) => {
                                    [vec![2], v.to_bits().to_le_bytes().to_vec()].concat()
                                }
                                ValueRef::Text(v) => [vec![3], v.to_vec()].concat(),
                                ValueRef::Blob(v) => [vec![4], v.to_vec()].concat(),
                            })
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .map(Result::unwrap),
        );
    }
    image
}

#[test]
fn database_claim_scalar_projection_original_large_nul_rows_do_not_copy_or_mutate() {
    for (table, column, prefix, restart) in [
        (
            "operation_journal_database_binding",
            "binding_id",
            "00000000-0000-4000-8000-000000000001",
            false,
        ),
        (
            "operation_journal_database_binding",
            "decision_nonce",
            "00000000-0000-4000-8000-000000000001",
            false,
        ),
        (
            "operation_journal_database_binding",
            "volume",
            "12345678901234567890",
            false,
        ),
        (
            "operation_journal_database_claim_slice",
            "slice_id",
            "00000000-0000-4000-8000-000000000001",
            false,
        ),
        (
            "operation_journal_ordinary_claim",
            "claim_id",
            "00000000-0000-4000-8000-000000000001",
            false,
        ),
        (
            "operation_journal_database_binding",
            "operation_id",
            "00000000-0000-4000-8000-000000000001",
            true,
        ),
        (
            "operation_journal_database_claim_slice",
            "operation_id",
            "00000000-0000-4000-8000-000000000001",
            true,
        ),
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (_, record) = persist_recording_database_fixture(&store, &f.owner);
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
            .unwrap();
        if restart && table.ends_with("slice") {
            store
                .connection
                .execute("DELETE FROM operation_journal_database_binding", [])
                .unwrap();
        }
        let value = format!("{prefix}\0{}", "x".repeat(1024 * 1024));
        assert_eq!(
            store
                .connection
                .query_row("SELECT length(?1)", [&value], |r| r.get::<_, usize>(0))
                .unwrap(),
            prefix.len()
        );
        store
            .connection
            .execute(
                &format!(
                    "UPDATE {table} SET {column}=?1 WHERE rowid=(SELECT rowid FROM {table} LIMIT 1)"
                ),
                [&value],
            )
            .unwrap();
        let before = scalar_database_image(&store);
        database_claims::SCALAR_COPIED_BYTES.with(|n| n.set(0));
        let result = if restart {
            store.validate_database_claims_inside_transaction()
        } else {
            store
                .read_database_claim_inventory(record.operation_id)
                .map(|_| ())
        };
        assert!(result.is_err(), "{table}.{column}");
        database_claims::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0, "{table}.{column}"));
        assert_eq!(scalar_database_image(&store), before);
    }
}

#[test]
fn database_claim_scalar_projection_original_restart_utf8_error_precedes_uuid_error() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    persist_recording_database_fixture(&store, &f.owner);
    store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
        .unwrap();
    let large = format!(
        "00000000-0000-4000-8000-000000000001\0{}",
        "x".repeat(1024 * 1024)
    );
    store
        .connection
        .execute(
            "UPDATE operation_journal_database_binding SET operation_id=?1 WHERE ordinal=0",
            [&large],
        )
        .unwrap();
    let mut invalid = vec![b'z'; 35];
    invalid.push(255);
    store.connection.execute("UPDATE operation_journal_database_binding SET operation_id=CAST(?1 AS TEXT) WHERE ordinal=1", [&invalid]).unwrap();
    let before = scalar_database_image(&store);
    let original = store.connection.prepare("SELECT DISTINCT operation_id FROM operation_journal_database_binding WHERE operation_id>'' ORDER BY operation_id LIMIT 64").unwrap()
        .query_map([], |row| row.get::<_, Option<String>>(0)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap_err();
    database_claims::SCALAR_COPIED_BYTES.with(|n| n.set(0));
    let actual = store
        .validate_database_claims_inside_transaction()
        .unwrap_err();
    assert!(
        actual.to_string().contains(&original.to_string()),
        "{actual}; original {original}"
    );
    database_claims::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    assert_eq!(scalar_database_image(&store), before);
}

#[test]
fn database_claim_scalar_projection_complete_original_inventory_and_nonce_still_read() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (_, record) = persist_recording_database_fixture(&store, &f.owner);
    // Exercise the original 64-row operation page boundary without truncation.
    for _ in 1..65 {
        persist_recording_database_fixture(&store, &f.owner);
    }
    let inventory = store
        .read_database_claim_inventory(record.operation_id)
        .unwrap()
        .unwrap();
    let before = scalar_database_image(&store);
    database_claims::SCALAR_COPIED_BYTES.with(|n| n.set(0));
    store.validate_database_claims_inside_transaction().unwrap();
    assert_eq!(
        store
            .read_database_claim_inventory(record.operation_id)
            .unwrap()
            .unwrap(),
        inventory
    );
    assert_eq!(
        store
            .read_database_claim_inventory_for_nonce(
                inventory.header().decision_nonce,
                inventory.digest().unwrap()
            )
            .unwrap()
            .unwrap(),
        (record.operation_id, inventory)
    );
    database_claims::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    assert_eq!(scalar_database_image(&store), before);
}
