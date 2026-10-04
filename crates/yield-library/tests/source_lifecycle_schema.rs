//! Normal-build actual global-owner schema installation; no lifecycle grants or effects.
#![cfg(not(feature = "test-support"))]

use rusqlite::{Connection, OpenFlags};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus, library::LIBRARY_DB_FILE_NAME,
    },
};

fn ready(writer: &GlobalLibraryWriter) -> GlobalLibraryWriterStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = writer.status();
        if status != GlobalLibraryWriterStatus::Initializing {
            return status;
        }
        assert!(Instant::now() < deadline, "actual owner startup timeout");
        thread::sleep(Duration::from_millis(5));
    }
}

fn start(guard: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(guard).unwrap();
    assert_eq!(
        ready(&writer),
        GlobalLibraryWriterStatus::Available,
        "{:?}",
        writer.unavailable_reason()
    );
    writer
}

fn read(path: &Path) -> Connection {
    Connection::open_with_flags(
        path.join(LIBRARY_DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn family_image(connection: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut statement = connection.prepare("SELECT type,name,sql FROM sqlite_schema WHERE tbl_name IN ('source_lifecycle_references','source_lifecycle_reference_receipts') ORDER BY name LIMIT 6").unwrap();
    statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn assert_empty_family(connection: &Connection) {
    assert_eq!(family_image(connection).len(), 5);
    for table in [
        "source_lifecycle_references",
        "source_lifecycle_reference_receipts",
    ] {
        assert_eq!(
            connection
                .query_row(
                    &format!("SELECT count(*) FROM (SELECT 1 FROM {table} LIMIT 1)"),
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

#[test]
fn actual_profile_owner_installs_empty_family_reopens_without_schema_changes_and_excludes_second_writer()
 {
    let root = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(root.path().to_path_buf()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start(&guard);
    assert!(
        GlobalLibraryWriter::start(&guard).is_err(),
        "named global lease excludes another writer"
    );
    let image = {
        let connection = read(root.path());
        assert_empty_family(&connection);
        family_image(&connection)
    };
    assert!(writer.shutdown());
    drop(writer);
    let (version, before_bytes) = {
        let connection = read(root.path());
        (
            connection
                .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            fs::read(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap(),
        )
    };
    let mut writer = start(&guard);
    assert_eq!(family_image(&read(root.path())), image);
    assert!(writer.shutdown());
    drop(writer);
    let connection = read(root.path());
    assert_empty_family(&connection);
    assert_eq!(
        version,
        connection
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    assert_eq!(
        before_bytes,
        fs::read(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap()
    );
}

#[test]
fn actual_legacy_global_migration_preserves_metadata_and_source_rows() {
    let root = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(root.path().to_path_buf()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    {
        let connection = Connection::open(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap();
        connection.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES ('retained-evidence','original'); CREATE TABLE sources(id TEXT PRIMARY KEY,root TEXT NOT NULL,sort_order INTEGER NOT NULL); INSERT INTO sources VALUES ('legacy-source','/original/media/root',7);").unwrap();
    }
    let mut writer = start(&guard);
    assert!(writer.shutdown());
    drop(writer);
    let connection = read(root.path());
    assert_empty_family(&connection);
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key='retained-evidence'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "original"
    );
    assert_eq!(connection.query_row("SELECT id,root,sort_order,role,metadata_storage,primary_import_folder FROM sources", [], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).unwrap(), ("legacy-source".into(),"/original/media/root".into(),7,"normal".into(),"source_folder".into(),"_Yield Inbox".into()));
}

#[test]
fn actual_owner_refuses_partial_or_altered_family_before_pragmas_or_unrelated_migration() {
    for mutation in [
        "DROP TABLE source_lifecycle_reference_receipts;",
        "DROP INDEX source_lifecycle_references_active_source;",
        "CREATE INDEX unexpected_index ON source_lifecycle_references(state);",
        "CREATE TRIGGER unexpected_trigger AFTER INSERT ON source_lifecycle_references BEGIN SELECT 1; END;",
        "ALTER TABLE source_lifecycle_references ADD COLUMN unexpected TEXT;",
    ] {
        let root = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(root.path().to_path_buf()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let mut writer = start(&guard);
        assert!(writer.shutdown());
        drop(writer);
        {
            let connection = Connection::open(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap();
            connection.execute_batch("PRAGMA journal_mode=DELETE; DROP TABLE profile_tag_vocabulary; INSERT INTO metadata VALUES ('retained-evidence','original');").unwrap();
            connection.execute_batch(mutation).unwrap();
        }
        let bytes = fs::read(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap();
        let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
        assert!(matches!(
            ready(&writer),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ));
        assert!(
            writer
                .unavailable_reason()
                .unwrap()
                .to_string()
                .contains("source lifecycle schema is incompatible")
        );
        writer.shutdown();
        drop(writer);
        assert_eq!(
            bytes,
            fs::read(root.path().join(LIBRARY_DB_FILE_NAME)).unwrap(),
            "{mutation}"
        );
        let connection = read(root.path());
        assert_eq!(
            connection
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name='profile_tag_vocabulary'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='retained-evidence'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "original"
        );
    }
}

#[cfg(unix)]
#[test]
fn replaced_profile_root_cannot_install_lifecycle_tables_into_substitute() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("profile");
    fs::create_dir(&root).unwrap();
    let _root = AppRootGuard::set(root.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let parked = parent.path().join("original");
    fs::rename(&root, &parked).unwrap();
    fs::create_dir(&root).unwrap();
    if let Ok(mut writer) = GlobalLibraryWriter::start(&guard) {
        assert!(matches!(
            ready(&writer),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ));
        writer.shutdown();
    }
    assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
    assert!(!parked.join(LIBRARY_DB_FILE_NAME).exists());
    fs::remove_dir(&root).unwrap();
    fs::rename(&parked, &root).unwrap();
}
