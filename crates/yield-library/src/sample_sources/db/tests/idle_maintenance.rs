//! Genuine retained SQLite/profile/lease cases, without capacity or retry authority.
use super::super::*;
use crate::app_dirs::AppRootGuard;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::path::Path;

fn database(body: impl FnOnce(&SourceDatabase, &Path, &WritableProfileGuard)) {
    let app = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _app = AppRootGuard::set(std::fs::canonicalize(app.path()).unwrap()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(root.path(), &guard, "idle-test")
        .unwrap();
    body(&db, root.path(), &guard);
}
fn pressure(db: &SourceDatabase) {
    db.connection
        .execute_batch(
            "PRAGMA wal_autocheckpoint=0;
         CREATE TABLE idle_payload(value BLOB);
         INSERT INTO idle_payload VALUES(zeroblob(34603008));",
        )
        .unwrap();
    assert!(
        crate::sqlite_wal::retained_wal_bytes(&db.connection).unwrap()
            >= crate::sqlite_wal::WAL_SOFT_WATERMARK_BYTES
    );
}
fn expiry(db: &SourceDatabase) -> i64 {
    db.connection
        .query_row("SELECT expires_at FROM source_lease", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn idle_source_wal_below_threshold_does_not_consume_original_checkpoint_attempt() {
    database(|db, _, _| {
        let control = db
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap();
        let expires = expiry(db);
        let changes = db.connection.total_changes();
        assert!(
            crate::sqlite_wal::retained_wal_bytes(&db.connection).unwrap()
                < crate::sqlite_wal::WAL_SOFT_WATERMARK_BYTES
        );
        db.maintain_idle_wal().unwrap();
        assert!(!db.checkpoint_policy.has_attempted());
        assert_eq!(expiry(db), expires);
        assert_eq!(db.connection.total_changes(), changes);
        assert_eq!(
            db.source_lease_control_total_changes_for_tests()
                .unwrap()
                .unwrap(),
            control
        );
        pressure(db);
        // The below-threshold observation did not spend the shared policy's attempt window.
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
    });
}

#[test]
fn idle_source_wal_near_expiry_never_renews_original_control_writer() {
    database(|db, _, _| {
        pressure(db);
        db.connection
            .execute("UPDATE source_lease SET expires_at = unixepoch() + 30", [])
            .unwrap();
        let expires = expiry(db);
        let control = db
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap();
        let changes = db.connection.total_changes();
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
        assert_eq!(expiry(db), expires);
        assert_eq!(db.connection.total_changes(), changes);
        assert_eq!(
            db.source_lease_control_total_changes_for_tests()
                .unwrap()
                .unwrap(),
            control
        );
        // A second idle call shares the exact postcommit policy's attempt throttle.
        let denied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts = denied.clone();
        db.connection
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Pragma {
                        pragma_name: "wal_checkpoint",
                        ..
                    }
                ) {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }));
        db.maintain_idle_wal().unwrap();
        assert_eq!(denied.load(std::sync::atomic::Ordering::Relaxed), 0);
        db.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    });
}

#[test]
fn idle_source_wal_expired_original_lease_refuses_without_checkpoint_or_renewal() {
    database(|db, _, _| {
        pressure(db);
        db.connection
            .execute(
                "UPDATE source_lease SET acquired_at = unixepoch() - 2, expires_at = unixepoch() - 1",
                [],
            )
            .unwrap();
        let expires = expiry(db);
        let control = db
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap();
        assert!(matches!(
            db.maintain_idle_wal(),
            Err(SourceAuthorityError::Database(SourceDbError::SourceLease(
                SourceLeaseError::Expired { .. }
            )))
        ));
        assert!(!db.checkpoint_policy.has_attempted());
        assert_eq!(expiry(db), expires);
        assert_eq!(
            db.source_lease_control_total_changes_for_tests()
                .unwrap()
                .unwrap(),
            control
        );
    });
}

#[test]
fn idle_source_wal_excludes_active_transactions_and_prepared_or_poisoned_policy() {
    database(|db, _, _| {
        pressure(db);
        for begin in [
            "BEGIN; SELECT singleton FROM source_lease",
            "BEGIN IMMEDIATE",
        ] {
            db.connection.execute_batch(begin).unwrap();
            db.maintain_idle_wal().unwrap();
            assert!(!db.checkpoint_policy.has_attempted());
            db.connection.execute_batch("ROLLBACK").unwrap();
        }
        for policy in [TAG_METADATA_POLICY_ENTERED, TAG_METADATA_POLICY_POISONED] {
            db.tag_metadata_policy_state.set(policy);
            assert!(matches!(
                db.maintain_idle_wal(),
                Err(SourceAuthorityError::Database(
                    SourceDbError::MetadataPolicyUnconfirmed
                ))
            ));
            assert!(!db.checkpoint_policy.has_attempted());
        }
        db.tag_metadata_policy_state.set(TAG_METADATA_POLICY_IDLE);
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
    });
}

#[test]
fn idle_source_wal_preserves_real_pinned_reader_and_denied_passive_result() {
    database(|db, _, _| {
        let reader = Connection::open(db.database_file_binding.as_ref().unwrap().path()).unwrap();
        reader
            .execute_batch("BEGIN; SELECT singleton FROM source_lease")
            .unwrap();
        pressure(db);
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
        let frames: (i64, i64) = db
            .connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert!(frames.0 > frames.1);
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            db.connection
                .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            34603008
        );
    });
    database(|db, _, _| {
        pressure(db);
        db.connection.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
        db.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(
            db.connection
                .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            34603008
        );
        // Ordinary original metadata writes remain usable after best-effort refusal.
        db.set_metadata("after-denial", "committed").unwrap();
    });
}

#[cfg(unix)]
#[test]
fn idle_source_wal_refuses_original_root_and_main_substitution() {
    database(|db, root, _| {
        pressure(db);
        let held = root.with_extension("held");
        std::fs::rename(root, &held).unwrap();
        std::fs::create_dir(root).unwrap();
        assert!(db.maintain_idle_wal().is_err());
        assert!(!db.checkpoint_policy.has_attempted());
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
        std::fs::rename(held, root).unwrap();
    });
    database(|db, _, _| {
        pressure(db);
        let main = db.database_file_binding.as_ref().unwrap().path();
        let held = main.with_extension("held");
        std::fs::rename(main, &held).unwrap();
        std::fs::write(main, b"foreign-original-main-refusal").unwrap();
        assert!(db.maintain_idle_wal().is_err());
        assert!(!db.checkpoint_policy.has_attempted());
        assert_eq!(
            std::fs::read(main).unwrap(),
            b"foreign-original-main-refusal"
        );
        std::fs::remove_file(main).unwrap();
        std::fs::rename(held, main).unwrap();
    });
}

#[test]
fn idle_source_wal_closed_original_gate_and_unavailable_journal_mode_do_not_checkpoint() {
    database(|db, _, _| {
        pressure(db);
        let control = db
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap();
        db.source_lease
            .as_ref()
            .unwrap()
            .effect_gate()
            .close_and_wait()
            .unwrap();
        assert!(matches!(
            db.maintain_idle_wal(),
            Err(SourceAuthorityError::ConfirmationClosed)
        ));
        assert!(!db.checkpoint_policy.has_attempted());
        assert_eq!(
            db.source_lease_control_total_changes_for_tests()
                .unwrap()
                .unwrap(),
            control
        );
    });
    database(|db, _, _| {
        pressure(db);
        db.connection.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma {
                    pragma_name: "journal_mode",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        db.maintain_idle_wal().unwrap();
        assert!(!db.checkpoint_policy.has_attempted());
        db.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        db.maintain_idle_wal().unwrap();
        assert!(db.checkpoint_policy.has_attempted());
    });
}

#[cfg(unix)]
#[test]
fn idle_source_wal_rechecks_original_root_after_an_admitted_native_pass() {
    database(|db, root, _| {
        pressure(db);
        let root = root.to_path_buf();
        let held = root.with_extension("held");
        let original = root.clone();
        let retained = held.clone();
        db.connection
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Pragma {
                        pragma_name: "wal_checkpoint",
                        ..
                    }
                ) {
                    std::fs::rename(&original, &retained).unwrap();
                    std::fs::create_dir(&original).unwrap();
                }
                Authorization::Allow
            }));
        let result = db.maintain_idle_wal();
        db.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(
            result.is_err(),
            "a completed observation cannot confirm replaced Source ownership"
        );
        assert!(db.checkpoint_policy.has_attempted());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(held, root).unwrap();
        assert_eq!(
            db.connection
                .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            34603008
        );
    });
}
