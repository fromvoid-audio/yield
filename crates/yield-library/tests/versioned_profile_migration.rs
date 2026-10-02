//! Real SQLite migration through normal-build profile-owned entrypoints (OPT-2095; parent OPT-1745).

#![cfg(not(feature = "test-support"))]

use std::{
    path::Path,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};
use tempfile::tempdir;
use yield_library::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    sample_sources::{
        DB_FILE_NAME, GlobalLibraryWriter, GlobalLibraryWriterStatus, SourceDatabase,
        SourceDbError, SourceLeaseError,
    },
};

const SOURCE_CHILD: &str = "catalog-source";
const PREFERENCES: &str = "{\"schema_version\":1,\"browser_zoom\":1.25}";

fn inspect(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("read-only evidence")
}

fn source_evidence(connection: &Connection) -> (i64, String, String, i64, i64) {
    let version = connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    let revision = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'revision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let (path, size, modified) = connection
        .query_row(
            "SELECT path, file_size, modified_ns FROM wav_files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    (version, revision, path, size, modified)
}

fn start_global(guard: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(guard).expect("profile-owned global writer");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match writer.status() {
            GlobalLibraryWriterStatus::Available => return writer,
            GlobalLibraryWriterStatus::Initializing => {
                assert!(Instant::now() < deadline, "global migration timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            status => panic!("global migration failed: {status:?}"),
        }
    }
}

fn assert_global(writer: &GlobalLibraryWriter, source_root: &Path) {
    let snapshot = writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.as_slice()[0].root, source_root);
    assert_eq!(snapshot.as_slice()[0].id.as_str(), "legacy-source");
    assert_eq!(
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .as_deref(),
        Some(PREFERENCES)
    );
}

#[test]
fn v11_profile_retains_actionable_recovery_and_global_migration_reopens_under_one_owner() {
    let profile = tempdir().unwrap();
    let source = tempdir().unwrap();
    let sibling = tempdir().unwrap();
    let sibling_file = sibling.path().join("profile-sentinel");
    std::fs::write(&sibling_file, b"unrelated profile remains untouched").unwrap();
    let _root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
    let child = profile.path().join(SOURCE_CHILD);
    std::fs::create_dir(&child).unwrap();
    let source_path = child.join(DB_FILE_NAME);
    let global_path = profile.path().join("library.db");
    // Raw writes only seed disposable fixtures before any production owner exists.
    let seed = Connection::open(&source_path).unwrap();
    seed.execute_batch(include_str!("fixtures/profile-v11/source.sql"))
        .unwrap();
    drop(seed);
    let seed = Connection::open(&global_path).unwrap();
    seed.execute_batch(include_str!("fixtures/profile-v11/library.sql"))
        .unwrap();
    seed.execute(
        "INSERT INTO sources VALUES ('legacy-source', ?1, 0)",
        [source.path().to_str().unwrap()],
    )
    .unwrap();
    drop(seed);
    let config_path = profile.path().join("config.toml");
    let config = b"[audio]\nvolume = 0.75\n";
    std::fs::write(&config_path, config).unwrap();

    let before = source_evidence(&inspect(&source_path));
    assert_eq!(before, (11, "7".into(), "legacy/one.wav".into(), 42, 99));
    let columns_before: Vec<String> = inspect(&global_path)
        .prepare("PRAGMA table_info(sources)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(columns_before, ["id", "root", "sort_order"]);
    let source_bytes = std::fs::read(&source_path).unwrap();
    let global_bytes = std::fs::read(&global_path).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    // Denial occurs before migration and leaves both fixture files byte-identical.
    assert!(matches!(
        SourceDatabase::open_for_maintenance(source.path()),
        Err(SourceDbError::ProfileOwnershipRequired)
    ));
    assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(std::fs::read(&global_path).unwrap(), global_bytes);

    let error = match SourceDatabase::open_for_catalog_with_profile_guard(
        source.path(),
        &guard,
        SOURCE_CHILD,
    ) {
        Ok(_) => panic!("legacy source adoption must remain unsupported"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            SourceDbError::SourceLease(SourceLeaseError::MigrationRequired {
                user_version: 11,
                ..
            })
        ),
        "unexpected recovery disposition: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("requires migration before source lease bootstrap")
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(source_evidence(&inspect(&source_path)), before);
    let lease_rows: i64 = inspect(&source_path)
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'source_lease'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        lease_rows, 0,
        "rejected migration must not mint source lease authority"
    );
    let mut writer = start_global(&guard);
    assert_global(&writer, source.path());
    let defaults: (String, String, String, i64) = inspect(&global_path).query_row("SELECT role, metadata_storage, primary_import_folder, sort_order FROM sources WHERE id = 'legacy-source'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!(
        defaults,
        (
            "normal".into(),
            "source_folder".into(),
            "_Yield Inbox".into(),
            0
        )
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), config);
    assert!(
        !source.path().join(DB_FILE_NAME).exists(),
        "migration must remain profile-local"
    );
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start_global(&guard);
    assert_global(&writer, source.path());
    assert_eq!(source_evidence(&inspect(&source_path)), before);
    assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(std::fs::read(&config_path).unwrap(), config);
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    WritableProfileGuard::acquire_current().expect("clean ownership handoff");
    assert_eq!(
        std::fs::read(&sibling_file).unwrap(),
        b"unrelated profile remains untouched"
    );
}
