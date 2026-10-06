//! Genuine original Source roots/lease/SQLite, without physical allocation qualification.
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};

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
