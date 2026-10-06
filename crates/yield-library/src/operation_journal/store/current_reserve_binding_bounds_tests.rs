#[test]
fn current_original_binding_rejects_large_borrowed_fields_before_owned_projection() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let volume = VolumeIdentity { device: 7 };
    let original = recovery_reserve_binding::ReserveBinding::from_live(
        volume.clone(),
        "profile",
        "root",
        "file",
        4096,
        1,
    )
    .unwrap();
    recovery_reserve_binding::persist(&store.connection, &original).unwrap();
    for field in [
        "profile_identity",
        "root_identity",
        "file_identity",
        "namespace_name",
    ] {
        // Original file-backed exclusive journal. Disable only fixture CHECK constraints so a
        // corrupted namespace field reaches the same bounded production row reader.
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        let sql = format!(
            "UPDATE {} SET {field}=?1 WHERE volume_device='7'",
            recovery_reserve_binding::TABLE_NAME
        );
        store
            .connection
            .execute(&sql, params!["x".repeat(1024 * 1024)])
            .unwrap();
        assert!(
            matches!(recovery_reserve_binding::read(&store.connection, &volume),
            Err(recovery_reserve_binding::ReserveBindingError::Corrupt(reason))
            if reason.ends_with("is outside its bounded size"))
        );
        store
            .connection
            .execute(
                &format!(
                    "DELETE FROM {} WHERE volume_device='7'",
                    recovery_reserve_binding::TABLE_NAME
                ),
                [],
            )
            .unwrap();
        recovery_reserve_binding::persist(&store.connection, &original).unwrap();
        assert_eq!(
            recovery_reserve_binding::read(&store.connection, &volume).unwrap(),
            Some(original.clone())
        );
    }
}
