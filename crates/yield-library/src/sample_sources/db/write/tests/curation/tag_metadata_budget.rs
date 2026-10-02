use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Optional extension initialization taints the entire process, including failed initialization.
// Exercise real owners in fresh processes instead of resetting that production safety fact.
pub(super) fn run_case(case: &str) {
    let test_name = concat!(module_path!(), "::tag_metadata_budget_child")
        .strip_prefix("yield_library::")
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .env("YIELD_TAG_METADATA_BUDGET_TEST_CASE", case)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_ENV)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_ENABLE_ENV)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_UNSAFE_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::Read;
            let mut output = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut output)
                .unwrap();
            assert!(
                status.success(),
                "budget fixture {case} failed: {status}\n{output}"
            );
            assert!(
                output.contains("test result: ok. 1 passed;"),
                "child did not execute the actual owner fixture: {output}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("budget fixture {case} exceeded bounded child deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn tag_metadata_budget_captures_real_facts_and_rejects_unknown_schema_or_handoff() {
    run_case("schema-and-handoff");
}
#[test]
fn tag_metadata_budget_respects_lower_existing_page_limit_and_remains_read_only() {
    run_case("existing-limit");
}
#[test]
fn tag_metadata_budget_rejects_real_oversized_database() {
    run_case("oversized");
}
#[test]
fn tag_metadata_budget_rejects_missing_shm_and_unsupported_sqlite_modes() {
    run_case("sidecars-and-mode");
}
#[test]
fn tag_metadata_budget_rejects_changed_revision_without_new_receipt() {
    run_case("revision");
}
#[test]
fn tag_metadata_budget_rejects_failed_extension_and_future_owners() {
    run_case("extension");
}
#[test]
fn tag_metadata_budget_rejects_unknown_runtime_collation() {
    run_case("runtime");
}

#[test]
#[ignore = "bounded subprocess driver, invoked by actual owner regressions"]
fn tag_metadata_budget_child() {
    let Ok(case) = std::env::var("YIELD_TAG_METADATA_BUDGET_TEST_CASE") else {
        return;
    };
    match case.as_str() {
        "schema-and-handoff" => schema_and_handoff(),
        "existing-limit" => existing_limit(),
        "oversized" => oversized(),
        "sidecars-and-mode" => sidecars_and_mode(),
        "revision" => revision(),
        "extension" => extension(),
        "runtime" => runtime(),
        "policy-restoration" => super::tag_metadata_policy::restoration(),
        "policy-full" => super::tag_metadata_policy::full(),
        "policy-poison" => super::tag_metadata_policy::poison(),
        "policy-restore-failure" => super::tag_metadata_policy::restore_failure(),
        _ => panic!("unknown fixture case"),
    }
}

fn assert_unknown(writer: &SourceWriter, revision: &crate::sample_sources::SourceRevision) {
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(matches!(
        writer.capture_tag_metadata_budget(revision),
        Err(crate::sample_sources::SourceWriterError::MetadataBoundNotDefined)
    ));
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
}

fn schema_and_handoff() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let before = writer.database_for_tests().connection.total_changes();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    assert_eq!(budget.revision(), &revision);
    assert_eq!(
        budget.page_size() * budget.maximum_pages(),
        64 * 1024 * 1024
    );
    assert!(budget.page_count() > 0);
    assert!(budget.resident_database_bytes() > 0);
    assert!(budget.required_metadata_bytes() > 128 * 1024 * 1024);
    writer.validate_tag_metadata_budget(&budget).unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        before
    );
    for (install, remove) in [
        (
            "CREATE TRIGGER unknown_metadata_effect AFTER INSERT ON wav_file_tags BEGIN INSERT OR REPLACE INTO metadata(key,value) VALUES ('tag-test-effect','unsafe'); END;",
            "DROP TRIGGER unknown_metadata_effect",
        ),
        (
            "CREATE TABLE unknown_table(value TEXT)",
            "DROP TABLE unknown_table",
        ),
        (
            "CREATE TEMP TABLE unknown_temp(value TEXT)",
            "DROP TABLE unknown_temp",
        ),
        (
            "ATTACH ':memory:' AS unknown_attached",
            "DETACH unknown_attached",
        ),
    ] {
        writer
            .database_for_tests()
            .connection
            .execute_batch(install)
            .unwrap();
        assert_unknown(&writer, &revision);
        writer
            .database_for_tests()
            .connection
            .execute_batch(remove)
            .unwrap();
    }
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    writer.validate_tag_metadata_budget(&budget).unwrap();
    let source = writer.into_catalog_database();
    let fresh = SourceWriter::new(source, SourceId::from_string("tag-budget"), 1).unwrap();
    assert!(matches!(
        fresh.validate_tag_metadata_budget(&budget),
        Err(crate::sample_sources::SourceWriterError::StaleFence)
    ));
}

fn existing_limit() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let first = writer.capture_tag_metadata_budget(&revision).unwrap();
    let limit = first.page_count() + 4;
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "max_page_count", limit)
        .unwrap();
    assert!(writer.validate_tag_metadata_budget(&first).is_err());
    let changes = writer.database_for_tests().connection.total_changes();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    assert_eq!(budget.maximum_pages(), limit);
    assert!(budget.required_metadata_bytes() < first.required_metadata_bytes());
    writer.validate_tag_metadata_budget(&budget).unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    let actual: u64 = writer
        .database_for_tests()
        .connection
        .pragma_query_value(None, "max_page_count", |r| r.get(0))
        .unwrap();
    assert_eq!(
        actual, limit,
        "observation must not install or raise a policy"
    );
}

fn oversized() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    connection.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO metadata(key,value) VALUES ('oversized-test',zeroblob(68157440)); DELETE FROM metadata WHERE key='oversized-test'; PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    assert!(crate::sqlite_wal::retained_database_bytes(connection).unwrap() > 64 * 1024 * 1024);
    assert_eq!(
        crate::sqlite_wal::retained_wal_bytes(connection).unwrap(),
        0
    );
    assert_unknown(&writer, &revision);
}

fn sidecars_and_mode() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let database = writer.database_for_tests();
    // SQLite still holds the original WAL-index mapping. A missing namespace is unknown,
    // never an inferred zero allocation, and no tag delta/receipt is entered.
    #[cfg(unix)]
    {
        let name = database.db_path.file_name().unwrap().to_string_lossy();
        let path = database.db_path.with_file_name(format!("{name}-shm"));
        let held = database.db_path.with_file_name(format!("{name}-shm-held"));
        std::fs::rename(&path, &held).unwrap();
        assert_unknown(&writer, &revision);
        std::fs::File::create(&path).unwrap();
        assert_unknown(&writer, &revision);
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&held, &path).unwrap();
    }
    database
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
        .unwrap();
    assert!(writer.capture_tag_metadata_budget(&revision).is_err());
    database
        .connection
        .execute_batch("PRAGMA journal_mode=WAL; SELECT value FROM metadata LIMIT 1")
        .unwrap();
    writer.capture_tag_metadata_budget(&revision).unwrap();
    database
        .connection
        .execute_batch("PRAGMA locking_mode=EXCLUSIVE")
        .unwrap();
    assert_unknown(&writer, &revision);
    database
        .connection
        .execute_batch("PRAGMA locking_mode=NORMAL")
        .unwrap();
}

fn revision() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_tag_metadata_budget(&revision).unwrap();
    writer
        .commit_catalog_curation_command(
            &revision,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_1,
            }],
        )
        .unwrap();
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(matches!(
        writer.validate_tag_metadata_budget(&budget),
        Err(crate::sample_sources::SourceWriterError::StaleFence)
    ));
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    writer
        .capture_tag_metadata_budget(&writer.current_revision().unwrap())
        .unwrap();
}

fn extension() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    writer.capture_tag_metadata_budget(&revision).unwrap();
    let file = fixture._source.path().join("invalid-extension");
    std::fs::write(&file, b"not a SQLite extension").unwrap();
    assert!(
        crate::sqlite_ext::load_extension_for_test(&writer.database_for_tests().connection, &file)
            .is_err()
    );
    assert_unknown(&writer, &revision);
    assert!(crate::sqlite_ext::try_extension_free_metadata_scope().is_none());
    let fresh = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("tag-budget"),
        1,
    )
    .unwrap();
    assert_unknown(&fresh, &revision);
}

fn runtime() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-budget"), 1).unwrap();
    let revision = writer.current_revision().unwrap();
    writer.capture_tag_metadata_budget(&revision).unwrap();
    unsafe extern "C" fn unknown_compare(
        _: *mut std::ffi::c_void,
        _: i32,
        _: *const std::ffi::c_void,
        _: i32,
        _: *const std::ffi::c_void,
    ) -> i32 {
        0
    }
    // SAFETY: real exclusively owned Connection, static name/callback, no user-data lifetime.
    // Registration remains owned by SQLite and is reclaimed with this connection.
    let status = unsafe {
        rusqlite::ffi::sqlite3_create_collation_v2(
            writer.database_for_tests().connection.handle(),
            c"unknown_tag_callback".as_ptr(),
            rusqlite::ffi::SQLITE_UTF8,
            std::ptr::null_mut(),
            Some(unknown_compare),
            None,
        )
    };
    assert_eq!(status, rusqlite::ffi::SQLITE_OK);
    assert_unknown(&writer, &revision);
}
