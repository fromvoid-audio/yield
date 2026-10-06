use super::*;

#[test]
fn no_effect_original_source_preparation_retains_exact_image_and_denies_sql_writes() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _root = crate::app_dirs::AppRootGuard::set(temp.path().to_path_buf()).unwrap();
    let child = temp.path().join("legacy");
    std::fs::create_dir(&child).unwrap();
    let path = child.join(super::super::DB_FILE_NAME);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/profile-v11/source.sql"
        )))
        .unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut owner =
        SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    let pages: u64 = owner
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    owner
        .connection
        .pragma_update(None, "max_page_count", pages + 1)
        .unwrap();
    let before = owner.capture(&profile).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(owner.connection.is_readonly(rusqlite::MAIN_DB).unwrap());
    owner.prepare_no_effect_writable(&profile).unwrap();
    assert!(!owner.connection.is_readonly(rusqlite::MAIN_DB).unwrap());
    assert!(owner.connection.has_no_effect_fence());
    assert_eq!(
        owner
            .connection
            .pragma_query_value(None, "max_page_count", |r| r.get::<_, u64>(0))
            .unwrap(),
        pages + 1
    );
    assert_eq!(owner.capture(&profile).unwrap(), before);
    owner.prepare_no_effect_writable(&profile).unwrap();
    owner
        .connection
        .execute_batch("PRAGMA query_only=OFF")
        .unwrap();
    assert!(
        owner
            .connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES('forbidden','write')",
                []
            )
            .is_err()
    );
    assert!(owner.validate(&profile).is_err());
    owner
        .connection
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    assert_eq!(owner.capture(&profile).unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    absent_sidecars(owner.binding.root()).unwrap();
}

#[test]
fn no_effect_refused_preparation_retains_original_readonly_custody() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _root = crate::app_dirs::AppRootGuard::set(temp.path().to_path_buf()).unwrap();
    let child = temp.path().join("legacy");
    std::fs::create_dir(&child).unwrap();
    let path = child.join(super::super::DB_FILE_NAME);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/profile-v11/source.sql"
        )))
        .unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut owner =
        SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    let original = owner.capture(&profile).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let sidecar = child.join(format!("{}-journal", super::super::DB_FILE_NAME));
    std::fs::write(&sidecar, b"retained evidence").unwrap();
    assert!(owner.prepare_no_effect_writable(&profile).is_err());
    assert!(owner.connection.is_readonly(rusqlite::MAIN_DB).unwrap());
    assert!(!owner.connection.has_no_effect_fence());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"retained evidence");
    std::fs::remove_file(&sidecar).unwrap(); // Test alone removes its injected bytes, never production.
    assert_eq!(owner.capture(&profile).unwrap(), original);
}

#[test]
fn no_effect_preparation_keeps_maximal_encoded_inputs_under_one_original_deadline() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let temp = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _root = crate::app_dirs::AppRootGuard::set(temp.path().to_path_buf()).unwrap();
        let child = temp.path().join("legacy");
        std::fs::create_dir(&child).unwrap();
        let path = child.join(super::super::DB_FILE_NAME);
        let mut connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "encoding", encoding)
            .unwrap();
        connection
            .execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/profile-v11/source.sql"
            )))
            .unwrap();
        connection.execute("DELETE FROM wav_files", []).unwrap();
        let transaction = connection.transaction().unwrap();
        let header = std::mem::size_of::<(String, String)>();
        let remaining = 65536 - ("revision".len() + 1 + header) - 127 * (4 + header);
        for n in 0..127 {
            let length = remaining / 127 + if n == 0 { remaining % 127 } else { 0 };
            transaction
                .execute(
                    "INSERT INTO metadata VALUES (?1,?2)",
                    rusqlite::params![format!("m{n:03}"), "v".repeat(length)],
                )
                .unwrap();
        }
        let average = 4096 - std::mem::size_of::<(String, i64, i64)>();
        for n in (0..1024).rev() {
            let length = average + if n == 0 { 40 } else { 0 } - if n == 1 { 40 } else { 0 };
            let relative = format!(
                "{n:04}/{}{}",
                "\u{e000}".repeat((length - 5) / 3),
                "x".repeat((length - 5) % 3)
            );
            assert_eq!(relative.len(), length);
            transaction
                .execute(
                    "INSERT INTO wav_files VALUES (?1,?2,?3)",
                    rusqlite::params![relative, n as i64, -(n as i64)],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(connection);
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let mut owner =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let original = owner.capture(&profile).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let pages: u64 = owner
            .connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        owner
            .connection
            .pragma_update(None, "max_page_count", pages + 1)
            .unwrap();
        // The real preparer captures original, private candidate and original again using
        // this ONE unchanged cooperative deadline; no fixture scalar grants a write.
        owner
            .prepare_no_effect_writable_until(&profile, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(owner.connection.has_no_effect_fence());
        assert!(!owner.connection.is_readonly(rusqlite::MAIN_DB).unwrap());
        assert_eq!(
            owner
                .connection
                .pragma_query_value(None, "max_page_count", |r| r.get::<_, u64>(0))
                .unwrap(),
            pages + 1
        );
        assert_eq!(owner.capture(&profile).unwrap(), original);
        assert_eq!(
            (original.metadata_rows(), original.audio_rows()),
            (128, 1024)
        );
        owner
            .connection
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        assert!(
            owner
                .connection
                .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
                .is_err()
        );
        owner
            .connection
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert_eq!(owner.capture(&profile).unwrap(), original);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        absent_sidecars(owner.binding.root()).unwrap();
    }
}
