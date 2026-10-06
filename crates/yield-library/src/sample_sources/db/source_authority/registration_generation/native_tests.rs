//! Genuine original Source roots/lease/SQLite, without physical allocation qualification.
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use std::sync::atomic::{AtomicU64, Ordering};

fn database(body: impl FnOnce(&SourceDatabase)) {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        "source-registration-generation",
    )
    .unwrap();
    body(&db);
}

#[test]
fn original_registered_source_request_carries_checked_generation() {
    database(|db| {
        let authority = db.prepare_source_authority().unwrap();
        let input = authority.journal_request().unwrap();
        let _proof = input
            .request
            .accept_confirmation(input.confirmation)
            .unwrap();
        let custody = authority.retain_registered_database_boundary(db).unwrap();
        custody.validate(db, &authority).unwrap();
        let alias = Arc::clone(&input.request);
        assert!(!input.request.database_file_identity().is_empty());
        assert!(authority.decision_boundary().is_some());
        let generation = input.request.registration_generation();
        assert!(generation > 0 && generation <= i64::MAX as u64);
        assert_eq!(custody.registration_generation(), generation);
        assert_eq!(alias.registration_generation(), generation);
        assert_eq!(input.request.registered_uses_for_tests(), 1);
        drop(custody);
        assert_eq!(input.request.registered_uses_for_tests(), 0);
        assert_eq!(alias.registration_generation(), generation);
        db.connection
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
    });
}

#[test]
fn original_source_generation_exhaustion_clears_only_failed_construction_marker() {
    database(|db| {
        let original_changes = db.connection.total_changes();
        let original_control = db.source_lease_control_total_changes_for_tests().unwrap();
        let original_expiry: i64 = db
            .connection
            .query_row("SELECT expires_at FROM source_lease", [], |row| row.get(0))
            .unwrap();
        for invalid in [0, i64::MAX as u64 + 1, u64::MAX] {
            let exhausted = AtomicU64::new(invalid);
            for _ in 0..2 {
                assert!(matches!(
                    db.prepare_source_authority_with_generation_counter(&exhausted),
                    Err(SourceAuthorityError::RegistrationGenerationExhausted)
                ));
                assert_eq!(exhausted.load(Ordering::Relaxed), invalid);
            }
        }
        assert_eq!(db.connection.total_changes(), original_changes);
        assert_eq!(
            db.source_lease_control_total_changes_for_tests().unwrap(),
            original_control
        );
        assert_eq!(db.connection.query_row::<i64, _, _>(
            "SELECT expires_at FROM source_lease", [], |row| row.get(0),
        ).unwrap(), original_expiry);
        db.connection
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        // This is the public genuine producer on the same original lease, not a forged clear.
        let authority = db.prepare_source_authority().unwrap();
        assert!(authority.registration_generation() > 0);
        let unused = AtomicU64::new(1);
        assert!(matches!(
            db.prepare_source_authority_with_generation_counter(&unused),
            Err(SourceAuthorityError::AlreadyPrepared)
        ));
        assert_eq!(unused.load(Ordering::Relaxed), 1);
        drop(authority);
        // Abandonment intentionally retains the successful singleton; exhaustion must not
        // provide a means to clear that original marker or issue a replacement registration.
        assert!(matches!(
            db.prepare_source_authority_with_generation_counter(&unused),
            Err(SourceAuthorityError::AlreadyPrepared)
        ));
        assert_eq!(unused.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn original_source_last_storage_generation_survives_registered_custody_and_aliases() {
    database(|db| {
        let last = AtomicU64::new(i64::MAX as u64);
        let authority = db
            .prepare_source_authority_with_generation_counter(&last)
            .unwrap();
        let input = authority.journal_request().unwrap();
        let _proof = input
            .request
            .accept_confirmation(input.confirmation)
            .unwrap();
        let custody = authority.retain_registered_database_boundary(db).unwrap();
        let alias = Arc::clone(&input.request);
        assert_eq!(alias.registration_generation(), i64::MAX as u64);
        assert_eq!(custody.registration_generation(), i64::MAX as u64);
        assert_eq!(last.load(Ordering::Relaxed), i64::MAX as u64 + 1);
        assert!(authority.decision_boundary().is_some());
        drop(custody);
        assert_eq!(alias.registered_uses_for_tests(), 0);
        assert_eq!(alias.registration_generation(), i64::MAX as u64);
    });
}
