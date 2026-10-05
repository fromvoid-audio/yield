//! Real owned SQLite corruption/refusal fixtures; no authenticated receipt or physical grant.
#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    use std::time::{Duration, Instant};

    const SEED: &str = "INSERT INTO source_lifecycle_references VALUES('00000000-0000-0000-0000-000000000001','shape-source',x'01','Enrolled','00000000-0000-0000-0000-000000000002','shape-work',zeroblob(32));
    INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,disposition,receipt) VALUES('00000000-0000-0000-0000-000000000002','shape-work',zeroblob(32),'00000000-0000-0000-0000-000000000003','00000000-0000-0000-0000-000000000001','Enrolled',x'02');";
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    fn settings(database: &LibraryDatabase) -> Vec<u64> {
        [
            "max_page_count",
            "temp_store",
            "cache_spill",
            "wal_autocheckpoint",
            "mmap_size",
            "ignore_check_constraints",
            "query_only",
        ]
        .into_iter()
        .map(|name| {
            database
                .connection
                .pragma_query_value(None, name, |r| r.get(0))
                .unwrap()
        })
        .collect()
    }
    fn seed(database: &LibraryDatabase) {
        database.connection.execute_batch(SEED).unwrap();
        assert_eq!(counts(database).unwrap(), (1, 1, 1));
        database.capture_lifecycle_policy_input(deadline()).unwrap();
    }
    fn refuse_without_changes(database: &LibraryDatabase) {
        let before = settings(database);
        let changes = database.connection.total_changes();
        assert_eq!(counts(database), Err(LifecycleAllocationError::Unknown));
        assert_eq!(
            database.capture_lifecycle_policy_input(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        assert!(matches!(
            super::super::super::policy::EnteredLifecyclePolicy::new(database, deadline()),
            Err(LifecycleAllocationError::Unknown)
        ));
        assert_eq!(before, settings(database));
        assert_eq!(changes, database.connection.total_changes());
        database
            .connection
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        assert_eq!(
            database.capture_lifecycle_allocation(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        assert_eq!(before, settings(database));
        assert_eq!(changes, database.connection.total_changes());
        database.connection.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn retained_reference_shapes_refuse_before_original_policy_without_eviction() {
        for mutation in [
            "registration_id=x'01'",
            "source_id=replace(hex(zeroblob(129)),'00','é')",
            "source_id='a'||char(0)||'b'",
            "subject=CAST(x'01' AS TEXT)",
            "subject=zeroblob(16385)",
            "subject=zeroblob(0)",
            "state='FutureState'",
            "last_operation_id='short'",
            "last_work_key='a'||char(0)||'b'",
            "last_work_key=replace(hex(zeroblob(129)),'00','é')",
            "row_identity=CAST(zeroblob(32) AS TEXT)",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
            let profile = WritableProfileGuard::acquire_current().unwrap();
            let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
            seed(&database);
            database
                .connection
                .execute_batch("PRAGMA ignore_check_constraints=ON")
                .unwrap();
            database
                .connection
                .execute_batch(&format!(
                    "UPDATE source_lifecycle_references SET {mutation}"
                ))
                .unwrap();
            database
                .connection
                .execute_batch("PRAGMA ignore_check_constraints=OFF")
                .unwrap();
            refuse_without_changes(&database);
            assert_eq!(
                database
                    .connection
                    .query_row(
                        "SELECT count(*) FROM source_lifecycle_references",
                        [],
                        |r| r.get::<_, u64>(0)
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                database
                    .connection
                    .query_row(
                        "SELECT count(*) FROM source_lifecycle_reference_receipts",
                        [],
                        |r| r.get::<_, u64>(0)
                    )
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn retained_receipt_shapes_refuse_before_original_policy_without_eviction() {
        for mutation in [
            "sequence=0",
            "operation_id='short'",
            "work_key='a'||char(0)||'b'",
            "work_key=replace(hex(zeroblob(129)),'00','é')",
            "payload_identity=zeroblob(31)",
            "owner_identity=x'01'",
            "registration_id='short'",
            "before_identity=zeroblob(31)",
            "after_identity=CAST(zeroblob(32) AS TEXT)",
            "disposition='FutureDisposition'",
            "receipt=CAST(x'01' AS TEXT)",
            "receipt=zeroblob(16385)",
            "receipt=zeroblob(0)",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
            let profile = WritableProfileGuard::acquire_current().unwrap();
            let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
            seed(&database);
            database
                .connection
                .execute_batch("PRAGMA ignore_check_constraints=ON")
                .unwrap();
            database
                .connection
                .execute_batch(&format!(
                    "UPDATE source_lifecycle_reference_receipts SET {mutation}"
                ))
                .unwrap();
            database
                .connection
                .execute_batch("PRAGMA ignore_check_constraints=OFF")
                .unwrap();
            refuse_without_changes(&database);
            assert_eq!(
                database
                    .connection
                    .query_row(
                        "SELECT count(*) FROM source_lifecycle_reference_receipts",
                        [],
                        |r| r.get::<_, u64>(0)
                    )
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn malformed_original_rows_survive_reopen_and_valid_nullable_shapes_still_prepare() {
        let temp = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
        seed(&database);
        // Both nullable identity forms are legal storage; neither authenticates a receipt.
        database.connection.execute_batch("UPDATE source_lifecycle_reference_receipts SET before_identity=zeroblob(32),after_identity=zeroblob(32)").unwrap();
        assert_eq!(counts(&database).unwrap(), (1, 1, 1));
        database.capture_lifecycle_policy_input(deadline()).unwrap();
        database.connection.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE source_lifecycle_references SET subject=zeroblob(16385); PRAGMA ignore_check_constraints=OFF").unwrap();
        let retained: Vec<u8> = database
            .connection
            .query_row("SELECT subject FROM source_lifecycle_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        refuse_without_changes(&database);
        drop(database);
        let reopened = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
        refuse_without_changes(&reopened);
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT subject FROM source_lifecycle_references", [], |r| r
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            retained
        );
        assert_eq!(counts(&reopened), Err(LifecycleAllocationError::Unknown));
    }
}
