//! Normal-build actual retained observer; fixtures confer no migration/allocation permission.
#![cfg(not(feature = "test-support"))]
use rusqlite::Connection;
use std::path::PathBuf;
use tempfile::{TempDir, tempdir};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{DB_FILE_NAME, SourceDatabase, SourceDbError, SourceLeaseError},
};

struct Fixture {
    _root: AppRootGuard,
    profile: TempDir,
    media: TempDir,
    guard: WritableProfileGuard,
    path: PathBuf,
}
impl Fixture {
    fn new(sql: &str) -> Self {
        let profile = tempdir().unwrap();
        let media = tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
        let child = profile.path().join("legacy");
        std::fs::create_dir(&child).unwrap();
        let path = child.join(DB_FILE_NAME);
        Connection::open(&path)
            .unwrap()
            .execute_batch(include_str!("fixtures/profile-v11/source.sql"))
            .unwrap();
        if !sql.is_empty() {
            Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        }
        let guard = WritableProfileGuard::acquire_current().unwrap();
        Self {
            _root: root,
            profile,
            media,
            guard,
            path,
        }
    }
    fn open(
        &self,
    ) -> Result<
        yield_library::sample_sources::db::LegacySourceReadOwner,
        yield_library::sample_sources::db::LegacyObservationError,
    > {
        SourceDatabase::open_legacy_read_owner(self.media.path(), &self.guard, "legacy")
    }
    fn lock(&self) -> PathBuf {
        self.profile.path().join("legacy/.yield-source-lease.lock")
    }
}
#[test]
fn retained_legacy_read_owner_captures_exact_image_preserves_bytes_and_never_bootstraps() {
    let f = Fixture::new("INSERT INTO metadata VALUES ('fixture-key','preserved')");
    let bytes = std::fs::read(&f.path).unwrap();
    let owner = f.open().unwrap();
    let image = owner.capture(&f.guard).unwrap();
    assert_eq!(image.revision(), 7);
    assert_eq!(image.metadata_rows(), 2);
    assert_eq!(image.audio_rows(), 1);
    assert_eq!(image.main_bytes(), bytes.len() as u64);
    assert_ne!(image.digest(), &[0; 32]);
    assert_eq!(image, owner.capture(&f.guard).unwrap());
    assert_eq!(std::fs::read(&f.path).unwrap(), bytes);
    assert!(
        f.open().is_err(),
        "retained exclusive reservation is not clonable"
    );
    drop(owner);
    assert!(
        f.lock().exists(),
        "Drop must preserve created sidecar evidence"
    );
    assert!(f.open().is_err(), "no reset of ambiguous prior sidecar");
    assert!(matches!(
        SourceDatabase::open_for_catalog_with_profile_guard(f.media.path(), &f.guard, "legacy"),
        Err(SourceDbError::SourceLease(
            SourceLeaseError::MigrationRequired {
                user_version: 11,
                ..
            }
        ))
    ));
    assert_eq!(std::fs::read(&f.path).unwrap(), bytes);
    let c = Connection::open(&f.path).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='source_lease'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        c.query_row(
            "SELECT path,file_size,modified_ns FROM wav_files",
            [],
            |r| Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?
            ))
        )
        .unwrap(),
        ("legacy/one.wav".into(), 42, 99)
    );
}
#[test]
fn every_unknown_schema_and_malformed_row_rejects_without_source_mutation() {
    for sql in [
        "PRAGMA user_version=12",
        "CREATE TABLE extra(a)",
        "CREATE TRIGGER hidden AFTER INSERT ON wav_files BEGIN DELETE FROM metadata; END",
        "CREATE INDEX extra ON wav_files(file_size)",
        "CREATE VIEW extra AS SELECT * FROM metadata",
        "CREATE TABLE source_lease(a)",
        "UPDATE metadata SET value='07' WHERE key='revision'",
        "UPDATE metadata SET value='18446744073709551615' WHERE key='revision'",
        "DELETE FROM metadata WHERE key='revision'",
        "INSERT INTO metadata VALUES('source.legacy_adoption.v1','unknown')",
        "UPDATE wav_files SET path='../escape.wav'",
        "UPDATE wav_files SET path='/absolute.wav'",
        "UPDATE wav_files SET path=printf('%05000d.wav',1)",
        "UPDATE wav_files SET file_size=-1",
        "UPDATE wav_files SET file_size='unknown'",
        "UPDATE wav_files SET path=CAST(X'FF' AS TEXT)",
        "UPDATE metadata SET value=zeroblob(65537)",
        "INSERT INTO metadata VALUES ('large-a',printf('%040000d',1)),('large-b',printf('%040000d',2))",
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<129) INSERT INTO metadata SELECT 'row-'||i,'value' FROM n",
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1025) INSERT INTO wav_files SELECT 'row-'||i||'.wav',1,1 FROM n",
    ] {
        let f = Fixture::new(sql);
        let bytes = std::fs::read(&f.path).unwrap();
        assert!(f.open().is_err(), "accepted {sql}");
        assert_eq!(std::fs::read(&f.path).unwrap(), bytes, "mutated {sql}");
    }
}

#[test]
fn original_image_is_frozen_even_if_an_external_sqlite_writer_ignores_yield_sidecar() {
    let f = Fixture::new("");
    let owner = f.open().unwrap();
    let original = owner.capture(&f.guard).unwrap();
    let external = Connection::open(&f.path).unwrap();
    external
        .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
        .unwrap();
    assert!(owner.capture(&f.guard).is_err());
    assert_eq!(original.revision(), 7);
    assert_eq!(
        external
            .query_row("SELECT value FROM metadata WHERE key='revision'", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "8"
    );
}
#[test]
fn sidecar_and_main_size_evidence_is_preserved_on_refusal() {
    for suffix in ["-wal", "-shm", "-journal", "lock"] {
        let f = Fixture::new("");
        let bytes = std::fs::read(&f.path).unwrap();
        let path = if suffix == "lock" {
            f.lock()
        } else {
            f.path.with_file_name(format!("{DB_FILE_NAME}{suffix}"))
        };
        std::fs::write(&path, b"ambiguous-evidence").unwrap();
        assert!(f.open().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"ambiguous-evidence");
        assert_eq!(std::fs::read(&f.path).unwrap(), bytes);
    }
    let f = Fixture::new("");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&f.path)
        .unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    let bytes = std::fs::read(&f.path).unwrap();
    assert!(f.open().is_err());
    assert_eq!(std::fs::read(&f.path).unwrap(), bytes);
}
#[test]
fn retained_owner_rejects_replaced_main_root_sidecar_and_foreign_profile() {
    for replacement in ["main", "media", "database-root", "lock"] {
        let f = Fixture::new("");
        let owner = f.open().unwrap();
        let path = match replacement {
            "main" => f.path.clone(),
            "media" => f.media.path().to_path_buf(),
            "database-root" => f.profile.path().join("legacy"),
            _ => f.lock(),
        };
        let backup = path.with_extension("held-original");
        std::fs::rename(&path, &backup).unwrap();
        if path == f.media.path() || replacement == "database-root" {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::copy(&backup, &path).unwrap();
        }
        assert!(owner.validate(&f.guard).is_err(), "missed {replacement}");
        assert!(
            owner.capture(&f.guard).is_err(),
            "captured replacement {replacement}"
        );
        drop(owner);
        if backup.is_dir() {
            std::fs::remove_dir_all(&backup).unwrap();
        }
    }
    let f = Fixture::new("");
    let owner = f.open().unwrap();
    let foreign = tempdir().unwrap();
    let _foreign_root = AppRootGuard::set(foreign.path().to_path_buf()).unwrap();
    let foreign_guard = WritableProfileGuard::acquire_current().unwrap();
    assert!(owner.validate(&foreign_guard).is_err());
    assert!(owner.capture(&foreign_guard).is_err());
}
