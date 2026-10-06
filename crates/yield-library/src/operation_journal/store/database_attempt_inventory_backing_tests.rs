// Real retained SQLite images, with synthetic observation-only attempt fixtures.
#[test]
fn database_attempt_inventory_backing_retains_oversized_original_image() {
    use crate::operation_journal::database_attempts::inventory_bytes::{MAX_CAPACITY, OWNED_BYTES};
    let f = fixture();
    let mut store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let valid = start(&record, &inventory, 0);
    store.persist_database_attempt_start(&valid).unwrap();
    assert_eq!(
        DatabaseAttemptStart::decode(&valid.encode().unwrap()).unwrap(),
        valid
    );
    let mut invalid = valid.clone();
    invalid.inventory = vec![0; 4 * crate::operation_journal::database_claims::MAX_INVENTORY_BYTES];
    let image = serde_json::to_vec(&invalid).unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal_database_attempt SET image=?1",
            [&image],
        )
        .unwrap();
    let before = retained_image(&store);
    for reopen in [false, true] {
        OWNED_BYTES.with(|n| n.set(0));
        MAX_CAPACITY.with(|n| n.set(0));
        let result = store.read_database_attempt(valid.attempt);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("attempt image shape")
        );
        assert!(
            OWNED_BYTES.with(|n| n.get())
                <= crate::operation_journal::database_claims::MAX_INVENTORY_BYTES + 1
        );
        assert!(
            MAX_CAPACITY.with(|n| n.get())
                <= crate::operation_journal::database_claims::MAX_INVENTORY_BYTES + 1
        );
        assert_eq!(retained_image(&store), before);
        assert!(store.connection.is_autocommit());
        if !reopen {
            drop(store);
            store = OperationJournalStore::open(&f.owner).unwrap();
        }
    }
}

#[test]
fn database_attempt_inventory_backing_preserves_original_error_order() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
    let mut invalid = start(&record, &inventory, 0);
    invalid.inventory = vec![0; crate::operation_journal::database_claims::MAX_INVENTORY_BYTES + 8];
    let canonical = serde_json::to_string(&invalid).unwrap();
    assert_eq!(
        DatabaseAttemptStart::decode(canonical.as_bytes())
            .unwrap_err()
            .to_string(),
        "attempt image shape"
    );
    for json in [
        canonical.replacen("\"inventory\":[0", "\"inventory\":[ 0", 1),
        format!(" {canonical}"),
        canonical.replace("\"schema\":", "\"schema\" :"),
    ] {
        assert_eq!(
            DatabaseAttemptStart::decode(json.as_bytes())
                .unwrap_err()
                .to_string(),
            "noncanonical attempt image"
        );
    }
    for json in [
        canonical.replacen(
            &format!("\"schema\":{}", invalid.schema),
            "\"schema\":\"bad\"",
            1,
        ),
        canonical.replacen("],\"schema\"", ",256],\"schema\"", 1),
        format!("{},\"schema\":1}}", &canonical[..canonical.len() - 1]),
    ] {
        assert_eq!(
            DatabaseAttemptStart::decode(json.as_bytes())
                .unwrap_err()
                .to_string(),
            "unsupported attempt image"
        );
    }
}
