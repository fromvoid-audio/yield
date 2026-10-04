//! Genuine file-backed owned SQLite restriction tests, never lifecycle effect grants.
#[cfg(all(test, target_os = "linux"))]
mod integration {
    use super::super::*;
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    fn fixture(test: impl FnOnce(&LibraryDatabase)) {
        let temp = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
        test(&database);
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    #[test]
    fn actual_policy_install_validate_and_restore_exact_original_settings_without_rows() {
        fixture(|database| {
            let before = Settings::read(&database.connection).unwrap();
            let changes = database.connection.total_changes();
            let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
            policy.install().unwrap();
            assert_eq!(
                Settings::read(&database.connection).unwrap(),
                policy.entered
            );
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap();
            policy.validate_reserved(deadline()).unwrap();
            database.connection.execute_batch("ROLLBACK").unwrap();
            policy.finish().unwrap();
            assert_eq!(Settings::read(&database.connection).unwrap(), before);
            assert_eq!(database.connection.total_changes(), changes);
        });
    }
    fn clear_authorizer(database: &LibraryDatabase) {
        database.connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
    }
    #[test]
    fn actual_lower_cap_and_nondefault_settings_are_restored_after_policy_unwind() {
        fixture(|database| {
            database.connection.execute_batch("PRAGMA max_page_count=2048; PRAGMA temp_store=FILE; PRAGMA cache_spill=1; PRAGMA wal_autocheckpoint=77; PRAGMA mmap_size=1048576").unwrap();
            let before = Settings::read(&database.connection).unwrap();
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
                assert_eq!(policy.allocation().selected_maximum_pages(), 2048);
                policy.install().unwrap();
                database
                    .connection
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap();
                policy.validate_reserved(deadline()).unwrap();
                panic!("injected owner unwind with original policy custody");
            }));
            assert!(panic.is_err());
            assert!(database.connection.is_autocommit());
            assert_eq!(Settings::read(&database.connection).unwrap(), before);
        });
    }
    #[test]
    fn each_actual_partial_install_denial_restores_prior_settings() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for name in [
            "max_page_count",
            "temp_store",
            "cache_spill",
            "wal_autocheckpoint",
            "mmap_size",
        ] {
            fixture(|database| {
                let before = Settings::read(&database.connection).unwrap();
                let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
                database.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
                    if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: Some(_) } if pragma_name.eq_ignore_ascii_case(name)) {
                        Authorization::Deny
                    } else { Authorization::Allow }
                }));
                assert!(policy.install().is_err(), "actual install {name}");
                clear_authorizer(database);
                policy.finish().unwrap();
                assert_eq!(
                    Settings::read(&database.connection).unwrap(),
                    before,
                    "restore after {name}"
                );
                assert!(database.connection.is_autocommit());
            });
        }
    }
    #[test]
    fn each_actual_restoration_denial_poison_refuses_reusable_policy() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for name in [
            "max_page_count",
            "temp_store",
            "cache_spill",
            "wal_autocheckpoint",
            "mmap_size",
        ] {
            fixture(|database| {
                let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
                policy.install().unwrap();
                database
                    .connection
                    .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                    .unwrap();
                database.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
                    if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: Some(_) } if pragma_name.eq_ignore_ascii_case(name)) {
                        Authorization::Deny
                    } else { Authorization::Allow }
                }));
                assert!(policy.finish().is_err(), "actual restoration {name}");
                clear_authorizer(database);
                assert!(
                    database
                        .connection
                        .pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))
                        .unwrap()
                );
                assert!(EnteredLifecyclePolicy::new(database, deadline()).is_err());
            });
        }
    }
    #[test]
    fn actual_unconfirmed_rollback_keeps_native_lock_and_never_restores_live_settings() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        fixture(|database| {
            let other = Connection::open(
                database
                    .preparation_root()
                    .unwrap()
                    .path()
                    .join(super::super::super::super::LIBRARY_DB_FILE_NAME),
            )
            .unwrap();
            other.busy_timeout(Duration::ZERO).unwrap();
            let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
            policy.install().unwrap();
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap();
            database.connection.authorizer(Some(|ctx: AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }));
            assert!(database.connection.execute_batch("ROLLBACK").is_err());
            assert!(policy.finish().is_err());
            assert!(!database.connection.is_autocommit());
            assert_eq!(
                other
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap_err()
                    .sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            assert_eq!(
                Settings::read(&database.connection).unwrap(),
                policy.entered
            );
            clear_authorizer(database);
            database.connection.execute_batch("ROLLBACK").unwrap();
        });
    }
    #[test]
    fn actual_stale_wal_input_and_modified_entered_settings_refuse() {
        fixture(|database| {
            let other = Connection::open(
                database
                    .preparation_root()
                    .unwrap()
                    .path()
                    .join(super::super::super::super::LIBRARY_DB_FILE_NAME),
            )
            .unwrap();
            let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
            other
                .execute(
                    "INSERT INTO metadata(key,value) VALUES ('policy-external','changed')",
                    [],
                )
                .unwrap();
            policy.install().unwrap();
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap();
            assert!(policy.validate_reserved(deadline()).is_err());
            database.connection.execute_batch("ROLLBACK").unwrap();
            policy.finish().unwrap();
            let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
            policy.install().unwrap();
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE; PRAGMA cache_spill=1")
                .unwrap();
            assert!(policy.validate_settings().is_err());
            database.connection.execute_batch("ROLLBACK").unwrap();
            policy.finish().unwrap();
        });
    }
    #[test]
    fn actual_unknown_schema_or_existing_transaction_refuses_before_settings_change() {
        fixture(|database| {
            let before = Settings::read(&database.connection).unwrap();
            database
                .connection
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap();
            assert!(EnteredLifecyclePolicy::new(database, deadline()).is_err());
            assert!(!database.connection.is_autocommit());
            assert_eq!(Settings::read(&database.connection).unwrap(), before);
            database
                .connection
                .execute_batch("ROLLBACK; CREATE TABLE unknown_policy(value)")
                .unwrap();
            assert!(EnteredLifecyclePolicy::new(database, deadline()).is_err());
            assert_eq!(Settings::read(&database.connection).unwrap(), before);
        });
    }
}
