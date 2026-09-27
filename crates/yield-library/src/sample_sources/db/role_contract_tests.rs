use std::path::Path;

use tempfile::tempdir;

use super::{DB_FILE_NAME, SourceDatabase, SourceDatabaseConnectionRole, SourceDbError};

#[test]
fn operational_roles_require_owned_maintenance_without_creating_or_migrating() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(DB_FILE_NAME);
    assert!(matches!(
        SourceDatabase::open_with_role(dir.path(), SourceDatabaseConnectionRole::JobWorker),
        Err(SourceDbError::MaintenanceRequired)
    ));
    assert!(!db_path.exists());

    let maintained = SourceDatabase::open_for_maintenance(dir.path()).expect("maintenance open");
    drop(maintained);
    let connection = rusqlite::Connection::open(&db_path).expect("schema stamp");
    connection.pragma_update(None, "user_version", 0).unwrap();
    drop(connection);
    let mut entries_before = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_before.sort();
    assert!(matches!(
        SourceDatabase::open_with_role(dir.path(), SourceDatabaseConnectionRole::JobWorker),
        Err(SourceDbError::MaintenanceRequired)
    ));
    let mut entries_after = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(entries_after, entries_before);
    let connection = rusqlite::Connection::open(&db_path).expect("schema unchanged");
    let stamp: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stamp, 0);
    drop(connection);

    drop(SourceDatabase::open_for_maintenance(dir.path()).expect("repair schema"));
    let connection = rusqlite::Connection::open(&db_path).expect("stamped database");
    connection
        .execute("DROP TABLE source_participant_curation_receipts", [])
        .expect("remove repairable relation");
    drop(connection);
    assert!(matches!(
        SourceDatabase::open_with_role(dir.path(), SourceDatabaseConnectionRole::JobWorker),
        Err(SourceDbError::MaintenanceRequired)
    ));
    drop(SourceDatabase::open_for_maintenance(dir.path()).expect("repair relation"));
    let connection = rusqlite::Connection::open(&db_path).expect("current-stamped database");
    connection
        .execute("ALTER TABLE wav_files DROP COLUMN last_curated_at", [])
        .expect("remove repairable column");
    drop(connection);
    assert!(matches!(
        SourceDatabase::open_with_role(dir.path(), SourceDatabaseConnectionRole::JobWorker),
        Err(SourceDbError::MaintenanceRequired)
    ));
    drop(SourceDatabase::open_for_maintenance(dir.path()).expect("repair column"));
    SourceDatabase::open_with_role(dir.path(), SourceDatabaseConnectionRole::JobWorker)
        .expect("compatible operational open");
}

#[test]
fn role_scoped_entrypoints_preserve_read_write_contracts() {
    let dir = tempdir().unwrap();
    let writer = SourceDatabase::open_for_source_write(dir.path()).unwrap();
    writer.upsert_file(Path::new("one.wav"), 10, 5).unwrap();

    let ui_read = SourceDatabase::open_for_ui_read(dir.path()).unwrap();
    assert_eq!(ui_read.count_files().unwrap(), 1);

    let background = SourceDatabase::open_for_background_job(dir.path()).unwrap();
    background
        .set_metadata("contract_probe", "background")
        .unwrap();

    let maintenance = SourceDatabase::open_for_maintenance(dir.path()).unwrap();
    assert_eq!(
        maintenance
            .get_metadata("contract_probe")
            .unwrap()
            .as_deref(),
        Some("background")
    );
}

#[test]
fn role_profiles_apply_expected_access_and_busy_timeouts() {
    let source_root = tempdir().unwrap();
    let database_root = tempdir().unwrap();
    let setup = SourceDatabase::open_for_source_write_with_database_root(
        source_root.path(),
        database_root.path(),
    )
    .unwrap();
    setup.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    drop(setup);

    for (role, expected_timeout_ms, writable) in [
        (SourceDatabaseConnectionRole::UiRead, 25, false),
        (SourceDatabaseConnectionRole::BackgroundRead, 5_000, false),
        (SourceDatabaseConnectionRole::JobWorker, 5_000, true),
        (SourceDatabaseConnectionRole::UserMetadataWrite, 5_000, true),
        (
            SourceDatabaseConnectionRole::PlaybackHistoryWrite,
            100,
            true,
        ),
        (SourceDatabaseConnectionRole::Maintenance, 5_000, true),
    ] {
        let database = SourceDatabase::open_with_role_and_database_root(
            source_root.path(),
            database_root.path(),
            role,
        )
        .unwrap();
        let busy_timeout_ms: u64 = database
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout_ms, expected_timeout_ms, "role: {role:?}");

        let write_result = database.set_metadata("role_probe", "value");
        assert_eq!(write_result.is_ok(), writable, "role: {role:?}");
    }
}
