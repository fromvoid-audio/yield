use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
#[cfg(unix)]
use crate::sample_sources::database_path_for;
use crate::sample_sources::db::file_ops_journal::BoundDatabaseRoot;
use crate::sample_sources::db::open::open_source_database_with_profile_guard_for_test;
#[cfg(unix)]
use crate::sample_sources::readiness::{
    ReadinessError, ReadinessStage, ReadinessTarget, ReadinessTargetPublication, SourceAvailability,
};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn source_database_identity_rejects_profile_legacy_layout_without_renaming() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-legacy-layout";
    let binding = BoundDatabaseRoot::for_profile_child(&profile_guard, Path::new(profile_child))
        .expect("profile child");
    let child_path = profile_guard.profile_root().join(profile_child);
    let legacy_path = child_path.join(LEGACY_DB_FILE_NAME);
    let legacy_bytes = b"legacy database bytes retained for migration";
    std::fs::write(&legacy_path, legacy_bytes).expect("legacy bytes");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::new(binding),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        None,
        None,
    ) {
        Ok(_) => panic!("profile-owned legacy migration must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::LegacyDatabaseMigrationRequiresBoundRoot { .. }
    ));
    assert_eq!(
        std::fs::read(&legacy_path).expect("retained legacy bytes"),
        legacy_bytes
    );
    assert!(!child_path.join(DB_FILE_NAME).exists());
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_sqlite_open() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-before-open";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("initial profile-owned database");
    drop(database);

    let child_path = profile_guard.profile_root().join(profile_child);
    let database_path = child_path.join(DB_FILE_NAME);
    let replacement_path = child_path.join(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement bytes must remain untouched";
    let gate_database_path = database_path.clone();
    let gate_replacement_path = replacement_path.clone();
    let gate = move |_opened_path: &Path| {
        std::fs::rename(&gate_database_path, &gate_replacement_path)
            .expect("displace original database");
        std::fs::write(&gate_database_path, replacement_bytes).expect("write replacement");
    };
    let binding = BoundDatabaseRoot::for_profile_child(&profile_guard, Path::new(profile_child))
        .expect("profile child");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::new(binding),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        None,
        Some(&gate),
    ) {
        Ok(_) => panic!("replacement before SQLite open must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[test]
fn source_database_identity_does_not_recreate_missing_original() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-missing-original",
    )
    .expect("profile-owned database");
    let expected_database = database
        .database_file_binding
        .as_ref()
        .expect("retained database identity")
        .clone();
    let database_path = profile_guard
        .profile_root()
        .join("identity-missing-original")
        .join(DB_FILE_NAME);
    drop(database);
    std::fs::remove_file(&database_path).expect("remove original database");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::clone(expected_database.root()),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        Some(&expected_database),
        None,
    ) {
        Ok(_) => panic!("missing original database must not be recreated"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(!database_path.exists());
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_readiness_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-before-readiness",
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join("identity-before-readiness")
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement readiness database remains unchanged";
    let mut readiness = database.readiness_store().expect("owned readiness store");
    std::fs::rename(&database_path, &displaced_path).expect("displace open database");
    std::fs::write(&database_path, replacement_bytes).expect("write replacement");

    let error = readiness
        .mark_temporarily_unavailable("identity-before-readiness", 1)
        .expect_err("replaced profile database must reject readiness mutation");
    assert!(matches!(
        error,
        ReadinessError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    drop(readiness);
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_during_readiness_write() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-during-readiness";
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join(profile_child)
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement during readiness write remains unchanged";
    database
        .connection
        .execute_batch(
            "CREATE TABLE source_database_identity_readiness_marker (
                 id INTEGER PRIMARY KEY,
                 writes INTEGER NOT NULL
             );
             INSERT INTO source_database_identity_readiness_marker (id, writes) VALUES (1, 0);
             CREATE TRIGGER source_database_identity_readiness_write
             AFTER INSERT ON source_readiness_sources
             BEGIN
                 UPDATE source_database_identity_readiness_marker
                 SET writes = writes + 1
                 WHERE id = 1;
             END;",
        )
        .expect("install readiness identity test hook");
    let hook_fired = std::sync::Arc::new(AtomicBool::new(false));
    let replacement_attempted = std::sync::Arc::new(AtomicBool::new(false));
    let replacement_written = std::sync::Arc::new(AtomicBool::new(false));
    let hook_fired_by_sqlite = std::sync::Arc::clone(&hook_fired);
    let attempted_by_sqlite = std::sync::Arc::clone(&replacement_attempted);
    let written_by_sqlite = std::sync::Arc::clone(&replacement_written);
    let callback_database_path = database_path.clone();
    let callback_displaced_path = displaced_path.clone();
    database.connection.update_hook(Some(
        move |_action: rusqlite::hooks::Action,
              _database_name: &str,
              table_name: &str,
              _row_id: i64| {
            if table_name == "source_database_identity_readiness_marker" {
                hook_fired_by_sqlite.store(true, Ordering::SeqCst);
                if !attempted_by_sqlite.swap(true, Ordering::SeqCst)
                    && std::fs::rename(&callback_database_path, &callback_displaced_path).is_ok()
                    && std::fs::write(&callback_database_path, replacement_bytes).is_ok()
                {
                    written_by_sqlite.store(true, Ordering::SeqCst);
                }
            }
        },
    ));
    let mut readiness = database.readiness_store().expect("owned readiness store");
    let target = ReadinessTarget::source(
        "identity-during-readiness",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target];

    let error = readiness
        .publish_targets(&ReadinessTargetPublication::new(
            "identity-during-readiness",
            1,
            1,
            SourceAvailability::Active,
            "readiness-contract-v1",
            &targets,
            1,
        ))
        .expect_err("replacement during readiness write must reject before commit");
    assert!(matches!(
        error,
        ReadinessError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(hook_fired.load(Ordering::SeqCst));
    assert!(replacement_written.load(Ordering::SeqCst));
    drop(readiness);
    let row_count = database
        .connection
        .query_row("SELECT COUNT(*) FROM source_readiness_sources", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read rolled-back readiness rows");
    assert_eq!(row_count, 0, "rejected readiness write must roll back");
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_unsafe_profile_sidecar() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-unsafe-sidecar";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned database");
    drop(database);

    let child_path = profile_guard.profile_root().join(profile_child);
    let database_path = child_path.join(DB_FILE_NAME);
    let sidecar_path = child_path.join(format!("{DB_FILE_NAME}-wal"));
    let outside = tempdir().expect("outside sidecar root");
    let outside_path = outside.path().join("outside-sidecar");
    let outside_bytes = b"outside sidecar bytes";
    std::fs::write(&outside_path, outside_bytes).expect("outside sidecar bytes");
    std::os::unix::fs::symlink(&outside_path, &sidecar_path).expect("sidecar symlink");

    let error = match SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    ) {
        Ok(_) => panic!("unsafe profile sidecar must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::UnsafeSidecar { .. })
    ));
    assert!(database_path.is_file());
    assert_eq!(
        std::fs::read(&outside_path).expect("outside sidecar bytes"),
        outside_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-before-mutation",
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join("identity-before-mutation")
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement database remains unchanged";
    std::fs::rename(&database_path, &displaced_path).expect("displace open database");
    std::fs::write(&database_path, replacement_bytes).expect("write replacement");

    let error = database
        .set_metadata("identity-replacement", "must-not-write")
        .expect_err("replaced profile database must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    let destination_root = tempdir().expect("snapshot destination root");
    let destination = database_path_for(destination_root.path());
    let snapshot_error = database
        .snapshot_to_path(&destination)
        .expect_err("replaced profile database must reject snapshot");
    assert!(matches!(
        snapshot_error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(
        !destination.exists(),
        "rejected snapshot must not create output"
    );
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}
