// Retained SQLite observations only: no permit or participant receipt authority.
#[test]
fn database_attempt_scalar_projection_retains_nul_corruption_without_copying() {
    for (table, column) in [
        (database_attempts::START_TABLE, "operation_id"),
        (database_attempts::START_TABLE, "binding_id"),
        (database_attempts::START_TABLE, "start_nonce"),
        (database_attempts::START_TABLE, "database_slice"),
        (database_attempts::START_TABLE, "sidecar_slice"),
        (database_attempts::OUTCOME_TABLE, "operation_id"),
        (database_attempts::OUTCOME_TABLE, "observation_id"),
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
        let a = start(&record, &inventory, 0);
        store.persist_database_attempt_start(&a).unwrap();
        let o = outcome(&a, AttemptOutcomeKind::Unknown);
        store.persist_database_attempt_outcome(&o).unwrap();
        let original: String = store
            .connection
            .query_row(&format!("SELECT {column} FROM {table}"), [], |r| r.get(0))
            .unwrap();
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        let corrupt = format!("{original}\0{}", "x".repeat(1024 * 1024));
        store
            .connection
            .execute(&format!("UPDATE {table} SET {column}=?1"), [&corrupt])
            .unwrap();
        let before = retained_image(&store);
        database_attempts::SCALAR_COPIED_BYTES.with(|b| b.set(0));
        let result = store.read_database_attempt(a.attempt);
        assert!(result.is_err(), "{table}.{column}");
        assert_eq!(
            database_attempts::SCALAR_COPIED_BYTES.with(|b| b.get()),
            0,
            "{table}.{column}"
        );
        assert_eq!(retained_image(&store), before);
        let retained: String = store
            .connection
            .query_row(&format!("SELECT {column} FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(retained, corrupt);
    }
}
#[test]
fn database_attempt_scalar_projection_nonce_and_audit_keys_remain_bounded() {
    for table in [
        database_attempts::START_TABLE,
        database_attempts::OUTCOME_TABLE,
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
        let a = start(&record, &inventory, 0);
        store.persist_database_attempt_start(&a).unwrap();
        store
            .persist_database_attempt_outcome(&outcome(&a, AttemptOutcomeKind::Unknown))
            .unwrap();
        assert!(
            store
                .read_database_attempt_for_nonce(a.nonce, a.digest().unwrap())
                .unwrap()
                .is_some()
        );
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        let corrupt = format!("{}\0{}", a.attempt, "x".repeat(1024 * 1024));
        store
            .connection
            .execute(&format!("UPDATE {table} SET attempt_id=?1"), [&corrupt])
            .unwrap();
        let before = retained_image(&store);
        database_attempts::SCALAR_COPIED_BYTES.with(|b| b.set(0));
        assert!(
            store
                .read_database_attempt_for_nonce(a.nonce, a.digest().unwrap())
                .is_err()
        );
        assert_eq!(database_attempts::SCALAR_COPIED_BYTES.with(|b| b.get()), 0);
        assert_eq!(retained_image(&store), before);
        assert!(OperationJournalStore::open(&f.owner).is_err());
    }
}
