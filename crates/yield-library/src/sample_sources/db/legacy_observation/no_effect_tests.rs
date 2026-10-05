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
