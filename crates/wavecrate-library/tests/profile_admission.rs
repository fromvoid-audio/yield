//! Normal-build checks for the profile-owned database admission boundary.

#![cfg(not(feature = "test-support"))]

use std::path::PathBuf;

use rusqlite::Connection;
use tempfile::{TempDir, tempdir};
use wavecrate_library::sample_sources::db::file_ops_journal;
use wavecrate_library::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    sample_sources::{
        DB_FILE_NAME, LibraryError, SourceDatabase, SourceDatabaseConnectionRole, SourceDbError,
        library::{self, LibraryState},
        readiness::{
            ReadinessClassification, ReadinessDeficit, ReadinessError, ReadinessStage,
            ReadinessStore, ReadinessTarget, ReadinessTargetPublication, SourceAvailability,
        },
    },
};

fn expect_source_profile_required(result: Result<SourceDatabase, SourceDbError>) {
    assert!(matches!(
        result,
        Err(SourceDbError::ProfileOwnershipRequired)
    ));
}

fn expect_connection_profile_required<T>(result: Result<T, SourceDbError>) {
    assert!(matches!(
        result,
        Err(SourceDbError::ProfileOwnershipRequired)
    ));
}

fn expect_library_profile_required<T>(result: Result<T, LibraryError>) {
    assert!(matches!(
        result,
        Err(LibraryError::ProfileOwnershipRequired)
    ));
}

fn assert_no_source_database_files_at(root: &std::path::Path) {
    for name in [
        DB_FILE_NAME,
        ".wavecrate_samples.db",
        ".wavecrate.db-wal",
        ".wavecrate.db-shm",
        ".wavecrate.db-journal",
    ] {
        assert!(
            !root.join(name).exists(),
            "unexpected source database artifact: {}",
            root.join(name).display()
        );
    }
}

fn assert_no_source_database_files(root: &TempDir) {
    assert_no_source_database_files_at(root.path());
}

fn assert_no_library_database_files(root: &TempDir) {
    for name in ["library.db", "library.db-wal", "library.db-shm"] {
        assert!(
            !root.path().join(name).exists(),
            "unexpected library database artifact: {}",
            root.path().join(name).display()
        );
    }
}

#[test]
fn path_only_writable_source_entrypoints_fail_before_filesystem_side_effects() {
    let source_root = tempdir().expect("source root");
    let database_parent = tempdir().expect("external database parent");
    let database_root = database_parent
        .path()
        .join("missing/external-database-root");

    expect_source_profile_required(SourceDatabase::open_for_source_write(source_root.path()));
    expect_source_profile_required(SourceDatabase::open_for_background_job(source_root.path()));
    expect_source_profile_required(SourceDatabase::open_for_scan(source_root.path()));
    expect_source_profile_required(SourceDatabase::open_for_maintenance(source_root.path()));
    expect_source_profile_required(SourceDatabase::open_for_user_metadata_write(
        source_root.path(),
    ));
    for role in [
        SourceDatabaseConnectionRole::JobWorker,
        SourceDatabaseConnectionRole::UserMetadataWrite,
        SourceDatabaseConnectionRole::PlaybackHistoryWrite,
        SourceDatabaseConnectionRole::Maintenance,
    ] {
        expect_source_profile_required(SourceDatabase::open_with_role(source_root.path(), role));
    }

    expect_source_profile_required(SourceDatabase::open_for_source_write_with_database_root(
        source_root.path(),
        &database_root,
    ));
    expect_source_profile_required(SourceDatabase::open_for_background_job_with_database_root(
        source_root.path(),
        &database_root,
    ));
    expect_source_profile_required(SourceDatabase::open_for_scan_with_database_root(
        source_root.path(),
        &database_root,
    ));
    expect_source_profile_required(
        SourceDatabase::open_for_user_metadata_write_with_database_root(
            source_root.path(),
            &database_root,
        ),
    );
    expect_source_profile_required(
        SourceDatabase::open_for_playback_history_write_with_database_root(
            source_root.path(),
            &database_root,
        ),
    );
    for role in [
        SourceDatabaseConnectionRole::JobWorker,
        SourceDatabaseConnectionRole::UserMetadataWrite,
        SourceDatabaseConnectionRole::PlaybackHistoryWrite,
        SourceDatabaseConnectionRole::Maintenance,
    ] {
        expect_source_profile_required(SourceDatabase::open_with_role_and_database_root(
            source_root.path(),
            &database_root,
            role,
        ));
    }
    expect_connection_profile_required(SourceDatabase::open_connection_for_background_job(
        source_root.path(),
    ));
    expect_connection_profile_required(SourceDatabase::open_connection_with_role(
        source_root.path(),
        SourceDatabaseConnectionRole::JobWorker,
    ));
    expect_connection_profile_required(
        SourceDatabase::open_connection_with_role_and_database_root(
            source_root.path(),
            &database_root,
            SourceDatabaseConnectionRole::JobWorker,
        ),
    );
    expect_connection_profile_required(
        SourceDatabase::open_unavailable_source_metadata_connection(
            &database_root,
            SourceDatabaseConnectionRole::JobWorker,
        ),
    );

    assert_no_source_database_files(&source_root);
    assert_no_source_database_files_at(&database_root);
    assert!(
        !database_root.exists(),
        "profile gate must precede parent creation"
    );
}

#[test]
fn path_only_global_library_entrypoints_fail_before_path_resolution() {
    let app_root = tempdir().expect("app root");
    let _app_root_guard = AppRootGuard::set(app_root.path().to_path_buf()).expect("app root");

    expect_library_profile_required(library::load());
    expect_library_profile_required(library::save(&LibraryState::default()));
    expect_library_profile_required(library::open_connection());

    assert_no_library_database_files(&app_root);
}

#[test]
fn readonly_and_profile_guarded_source_entrypoints_remain_available() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path.clone()).expect("app root");
    let profile_child = PathBuf::from("catalog-readonly");
    let database = {
        let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
        SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &profile_guard,
            &profile_child,
        )
        .expect("guarded catalog database")
    };
    let owned_recovery =
        file_ops_journal::reconcile_pending_ops(&database).expect("profile-owned empty recovery");
    assert_eq!(owned_recovery.total, 0);
    assert_eq!(owned_recovery.completed, 0);
    assert!(owned_recovery.errors.is_empty());
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    database
        .snapshot_to_path(&source_root.path().join("snapshot.db"))
        .expect("guarded snapshot");

    let fence = database
        .snapshot_to_path_with_write_fence(&source_root.path().join("fenced-snapshot.db"))
        .expect("guarded snapshot fence");
    drop(database);
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    drop(fence);
    let reacquired = WritableProfileGuard::acquire_current().expect("released profile owner");

    let database_root = app_root_path.join(&profile_child);
    let read_only =
        SourceDatabase::open_for_ui_read_with_database_root(source_root.path(), &database_root)
            .expect("readonly database");
    assert_eq!(read_only.count_files().expect("readonly count"), 0);
    let readonly_recovery = file_ops_journal::reconcile_pending_ops(&read_only)
        .expect_err("readonly recovery must fail before filesystem effects");
    assert_eq!(
        readonly_recovery,
        SourceDbError::ProfileOwnershipRequired.to_string()
    );
    let read_only_snapshot = source_root.path().join("readonly-snapshot.db");
    assert!(matches!(
        read_only.snapshot_to_path(&read_only_snapshot),
        Err(SourceDbError::ProfileOwnershipRequired)
    ));
    assert!(!read_only_snapshot.exists());
    let read_only_role = SourceDatabase::open_with_role_and_database_root(
        source_root.path(),
        &database_root,
        SourceDatabaseConnectionRole::BackgroundRead,
    )
    .expect("readonly role database");
    assert_eq!(
        read_only_role.count_files().expect("readonly role count"),
        0
    );
    let read_only_connection = SourceDatabase::open_connection_with_role_and_database_root(
        source_root.path(),
        &database_root,
        SourceDatabaseConnectionRole::UiRead,
    )
    .expect("readonly raw connection");
    assert!(
        read_only_connection
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .expect("readonly query")
            == 1
    );
    drop(reacquired);
    assert!(
        !source_root.path().join(DB_FILE_NAME).exists(),
        "guarded external database must not create a source-local database"
    );
}

#[test]
fn snapshot_fence_retains_profile_child_lease_after_database_drop() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root");
    let profile_child = PathBuf::from("catalog-fence-lease");
    let (database, probe_guard) = {
        let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
        let probe_guard = profile_guard.try_clone().expect("profile owner probe");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &profile_guard,
            &profile_child,
        )
        .expect("guarded catalog database");
        (database, probe_guard)
    };

    let destination = source_root.path().join("fenced-lease-snapshot.db");
    let fence = database
        .snapshot_to_path_with_write_fence(&destination)
        .expect("guarded snapshot fence");
    drop(database);

    let duplicate = match SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &probe_guard,
        &profile_child,
    ) {
        Ok(_) => panic!("snapshot fence must retain the profile child lease"),
        Err(error) => error,
    };
    assert!(matches!(
        duplicate,
        SourceDbError::ProfileOwnership(ProfileOwnershipError::ProfileChildAlreadyLeased { .. })
    ));

    drop(fence);
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &probe_guard,
        &profile_child,
    )
    .expect("profile child lease released with snapshot fence");
    drop(reopened);
    drop(probe_guard);
    WritableProfileGuard::acquire_current().expect("profile owner released");
}

#[cfg(unix)]
#[test]
fn guarded_snapshot_rejects_profile_root_replacement_before_destination_creation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path.clone()).expect("app root");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "catalog-root-replacement",
    )
    .expect("guarded catalog database");
    let destination = source_root.path().join("replaced-root-snapshot.db");
    let parked_root = app_root_path.with_file_name(format!(
        "{}-parked",
        app_root_path
            .file_name()
            .expect("app root name")
            .to_string_lossy()
    ));

    std::fs::rename(&app_root_path, &parked_root).expect("park original profile root");
    std::fs::create_dir(&app_root_path).expect("replacement profile root");
    let error = database
        .snapshot_to_path(&destination)
        .expect_err("replaced profile root must reject guarded snapshot");
    assert!(matches!(
        error,
        SourceDbError::ProfileOwnership(ProfileOwnershipError::ProfileRootReplaced { .. })
    ));
    assert!(!destination.exists());

    std::fs::remove_dir(&app_root_path).expect("remove replacement profile root");
    std::fs::rename(&parked_root, &app_root_path).expect("restore original profile root");
}

#[test]
fn readonly_open_reports_missing_database_without_creating_one() {
    let source_root = tempdir().expect("source root");

    assert!(matches!(
        SourceDatabase::open_for_ui_read(source_root.path()),
        Err(SourceDbError::ReadOnlyDatabaseMissing(path))
            if path == source_root.path().join(DB_FILE_NAME)
    ));
    assert_no_source_database_files(&source_root);
}

#[test]
fn readiness_ownership_raw_mutations_fail_without_side_effects() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path.clone()).expect("app root override");
    let profile_child = "readiness-ownership";
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned source database");

    let target = ReadinessTarget::source(
        "readiness-ownership-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target.clone()];
    let publication = ReadinessTargetPublication::new(
        "readiness-ownership-source",
        1,
        1,
        SourceAvailability::Active,
        "readiness-contract-v1",
        &targets,
        1,
    );

    {
        let mut owned_store = database.readiness_store().expect("owned readiness store");
        owned_store
            .publish_targets(&publication)
            .expect("owned readiness publication");
        owned_store
            .persist_deficits(
                &[ReadinessDeficit {
                    target: target.clone(),
                    reason: ReadinessClassification::Pending,
                    enqueued_at: None,
                }],
                1,
            )
            .expect("owned readiness work queue");
        assert!(
            owned_store
                .source_exists("readiness-ownership-source")
                .expect("read readiness source")
        );
    }

    let database_path = app_root_path.join(profile_child).join(DB_FILE_NAME);
    let mut raw_connection = Connection::open(&database_path).expect("raw readiness connection");
    let row_count = |connection: &Connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM source_readiness_sources WHERE source_id = ?1",
                ["readiness-ownership-source"],
                |row| row.get::<_, i64>(0),
            )
            .expect("read readiness row count")
    };
    assert_eq!(row_count(&raw_connection), 1);
    {
        let mut raw_store = ReadinessStore::new(&mut raw_connection);
        assert!(
            raw_store
                .source_exists("readiness-ownership-source")
                .expect("raw readiness existence query")
        );
        assert_eq!(
            raw_store
                .source_state("readiness-ownership-source")
                .expect("raw readiness state query")
                .expect("raw readiness source state")
                .availability,
            SourceAvailability::Active
        );
    }

    let raw_publish_error = ReadinessStore::new(&mut raw_connection)
        .publish_targets(&publication)
        .expect_err("raw readiness publication must require profile ownership");
    assert!(matches!(
        raw_publish_error,
        ReadinessError::OwnershipRequired
    ));
    assert_eq!(row_count(&raw_connection), 1);

    let job_state = |connection: &Connection| {
        connection
            .query_row(
                "SELECT status, readiness_claim_generation, lease_expires_at
                 FROM analysis_jobs
                 WHERE source_id = ?1
                   AND readiness_managed = 1
                   AND readiness_scope_kind = 'source'
                   AND readiness_scope_id = ?1
                   AND readiness_stage = 'similarity_layout'",
                ["readiness-ownership-source"],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .expect("read readiness claim state")
    };
    let pending_job_state = job_state(&raw_connection);
    assert_eq!(
        pending_job_state,
        (String::from("pending"), 0, None),
        "published target should start unclaimed"
    );
    let raw_claim_error = ReadinessStore::new(&mut raw_connection)
        .claim(&targets[0], 4, 60)
        .expect_err("raw readiness claim must require profile ownership");
    assert!(matches!(raw_claim_error, ReadinessError::OwnershipRequired));
    assert_eq!(
        job_state(&raw_connection),
        pending_job_state,
        "rejected raw claim must not change durable claim state"
    );

    let raw_error = ReadinessStore::new(&mut raw_connection)
        .mark_temporarily_unavailable("readiness-ownership-source", 2)
        .expect_err("raw readiness mutation must require profile ownership");
    assert!(matches!(raw_error, ReadinessError::OwnershipRequired));
    assert_eq!(row_count(&raw_connection), 1);
    assert_eq!(
        raw_connection
            .query_row(
                "SELECT availability FROM source_readiness_sources WHERE source_id = ?1",
                ["readiness-ownership-source"],
                |row| row.get::<_, String>(0),
            )
            .expect("read unchanged readiness state"),
        "active"
    );

    let mut owned_store = database.readiness_store().expect("owned readiness store");
    owned_store
        .mark_temporarily_unavailable("readiness-ownership-source", 3)
        .expect("owned readiness mutation");
    assert_eq!(
        owned_store
            .source_state("readiness-ownership-source")
            .expect("read owned readiness state")
            .expect("source state")
            .availability,
        SourceAvailability::Offline
    );
}
