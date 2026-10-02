use super::tag_metadata_budget::run_case;
use super::*;

#[test]
fn tag_metadata_policy_enforces_and_restores_real_owner_settings() {
    run_case("policy-restoration");
}
#[test]
fn tag_metadata_policy_real_sqlite_full_rolls_back_under_existing_page_cap() {
    run_case("policy-full");
}
#[test]
fn tag_metadata_policy_nonquiescent_finish_poisons_owner_and_handoff() {
    run_case("policy-poison");
}

#[test]
fn tag_metadata_policy_failed_sqlite_setting_restoration_retains_poison() {
    run_case("policy-restore-failure");
}

fn settings(db: &SourceDatabase) -> Vec<u64> {
    [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
    ]
    .map(|name| {
        db.connection
            .pragma_query_value(None, name, |r| r.get(0))
            .unwrap()
    })
    .to_vec()
}

pub(super) fn restoration() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(fixture.database, SourceId::from_string("policy"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    // Force an actual healthy lease into the ordinary renewal window. Entered policy must
    // validate it without issuing an uncapped write on the separate control connection.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 45],
        )
        .unwrap();
    let expires_before: i64 = writer
        .database_for_tests()
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    let original = settings(writer.database_for_tests());
    let changes = writer.database_for_tests().connection.total_changes();
    let policy = writer.begin_tag_metadata_policy(&budget).unwrap();
    assert_eq!(
        settings(writer.database_for_tests()),
        vec![budget.maximum_pages(), 2, 0, 0]
    );
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    policy.validate_entered().unwrap();
    assert!(writer.begin_tag_metadata_policy(&budget).is_err());
    assert!(writer.capture_tag_metadata_budget(&revision).is_err());
    // Actual owner transaction; successful scalar metadata commit only tests runtime policy.
    // It grants no tag/capacity/journal authority and is not a tag participant completion.
    let mut batch = writer.database_for_tests().write_batch().unwrap();
    batch
        .set_metadata("tag-policy-fixture", "committed")
        .unwrap();
    policy.validate_entered().unwrap();
    batch.commit_auxiliary_state().unwrap();
    let expires_entered: i64 = writer
        .database_for_tests()
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        expires_entered, expires_before,
        "entered policy must not renew through another connection"
    );
    policy.finish().unwrap();
    assert_eq!(settings(writer.database_for_tests()), original);
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert_eq!(
        writer
            .database_for_tests()
            .get_metadata("tag-policy-fixture")
            .unwrap()
            .as_deref(),
        Some("committed")
    );
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    let policy = writer.begin_tag_metadata_policy(&budget).unwrap();
    let mut batch = writer.database_for_tests().write_batch().unwrap();
    batch
        .set_metadata("tag-policy-fixture", "rolled-back")
        .unwrap();
    drop(batch);
    drop(policy);
    assert_eq!(settings(writer.database_for_tests()), original);
    assert_eq!(
        writer
            .database_for_tests()
            .get_metadata("tag-policy-fixture")
            .unwrap()
            .as_deref(),
        Some("committed")
    );
    writer.capture_tag_metadata_budget(&revision).unwrap();
}

pub(super) fn full() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(fixture.database, SourceId::from_string("policy"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let first = writer.capture_tag_metadata_budget(&revision).unwrap();
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "max_page_count", first.page_count() + 2)
        .unwrap();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    let original = settings(writer.database_for_tests());
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let policy = writer.begin_tag_metadata_policy(&budget).unwrap();
    let batch = writer.database_for_tests().write_batch().unwrap();
    let error = batch
        .curation_transaction()
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('tag-policy-full',zeroblob(524288))",
            [],
        )
        .unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(ref e, _) if e.code == rusqlite::ErrorCode::DiskFull)
    );
    // SQLITE_FULL may itself roll back the SQLite transaction; explicit batch drop is still
    // completed before restoring settings. No delta/revision/count receipt may be fabricated.
    drop(batch);
    policy.finish().unwrap();
    assert_eq!(settings(writer.database_for_tests()), original);
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
    assert!(
        writer
            .database_for_tests()
            .get_metadata("tag-policy-full")
            .unwrap()
            .is_none()
    );
    writer.capture_tag_metadata_budget(&revision).unwrap();
}

pub(super) fn poison() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(fixture.database, SourceId::from_string("policy"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let policy = writer.begin_tag_metadata_policy(&budget).unwrap();
    let batch = writer.database_for_tests().write_batch().unwrap();
    assert!(matches!(
        policy.finish(),
        Err(crate::sample_sources::SourceWriterError::Database(
            SourceDbError::MetadataPolicyUnconfirmed
        ))
    ));
    drop(batch);
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
    assert!(matches!(
        writer.database_for_tests().write_batch(),
        Err(SourceDbError::MetadataPolicyUnconfirmed)
    ));
    assert!(writer.capture_tag_metadata_budget(&revision).is_err());
    let source = writer.into_catalog_database();
    let reopened_writer = SourceWriter::new(source, SourceId::from_string("policy"), 1).unwrap();
    assert!(
        reopened_writer
            .capture_tag_metadata_budget(&revision)
            .is_err()
    );
    assert!(matches!(
        reopened_writer.database_for_tests().write_batch(),
        Err(SourceDbError::MetadataPolicyUnconfirmed)
    ));
    // Handing the same connection to another wrapper is not actual close/reopen recovery.
    let database = reopened_writer.into_catalog_database();
    let profile_child = Path::new("curation-profile-bound");
    database.close().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        profile_child,
    )
    .unwrap();
    let fresh = SourceWriter::new(database, SourceId::from_string("policy"), 1).unwrap();
    let fresh_revision = fresh.current_revision().unwrap();
    fresh.capture_tag_metadata_budget(&fresh_revision).unwrap();
    assert!(fresh.database_for_tests().write_batch().is_ok());
}

pub(super) fn restore_failure() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(fixture.database, SourceId::from_string("policy"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    let policy = writer.begin_tag_metadata_policy(&budget).unwrap();
    // Fail a real SQLite PRAGMA during restoration, after the scope has quiesced. This is
    // a SQL failure seam, not simulated disk qualification or a fabricated capacity grant.
    unsafe extern "C" fn deny_temp_store(
        _: *mut std::ffi::c_void,
        action: i32,
        name: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == rusqlite::ffi::SQLITE_PRAGMA && !name.is_null() {
            // SAFETY: SQLite supplies a valid nul-terminated action argument during this callback.
            if unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes() == b"temp_store" {
                return rusqlite::ffi::SQLITE_DENY;
            }
        }
        rusqlite::ffi::SQLITE_OK
    }
    // SAFETY: callback/userdata have static lifetime, connection exclusively held by fixture.
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                writer.database_for_tests().connection.handle(),
                Some(deny_temp_store),
                std::ptr::null_mut(),
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    assert!(matches!(
        policy.finish(),
        Err(crate::sample_sources::SourceWriterError::Database(
            SourceDbError::MetadataPolicyUnconfirmed
        ))
    ));
    // SAFETY: remove the callback before cleanup; no concurrent SQLite calls.
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                writer.database_for_tests().connection.handle(),
                None,
                std::ptr::null_mut(),
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    let raw_write = writer.database_for_tests().connection.execute(
        "INSERT INTO metadata(key,value) VALUES ('poison-raw-write','no')",
        [],
    );
    assert!(
        matches!(raw_write, Err(rusqlite::Error::SqliteFailure(ref e, _)) if e.code == rusqlite::ErrorCode::ReadOnly)
    );
    assert!(matches!(
        writer.database_for_tests().write_batch(),
        Err(SourceDbError::MetadataPolicyUnconfirmed)
    ));
    assert!(writer.capture_tag_metadata_budget(&revision).is_err());
}
