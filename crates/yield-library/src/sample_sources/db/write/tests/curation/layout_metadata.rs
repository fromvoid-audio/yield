use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KEY: &str = "source_starmap_generation_v1";
macro_rules! owned {
    ($fixture:ident, $writer:ident) => {
        let $fixture = CurationFixture::new_profile_bound();
        let $writer =
            SourceWriter::new($fixture.database, SourceId::from_string("layout-policy"), 1)
                .unwrap();
    };
}
fn run_case(case: &str) {
    let name = concat!(module_path!(), "::layout_metadata_child")
        .strip_prefix("yield_library::")
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("YIELD_LAYOUT_METADATA_CASE", case)
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
            assert!(status.success(), "layout case {case}: {status}\n{output}");
            assert!(
                output.contains("test result: ok. 1 passed;"),
                "case did not run: {output}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("layout case {case} exceeded subprocess deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case_tests {
    ($($name:ident => $case:literal),* $(,)?) => {$ (
        #[test] fn $name() { run_case($case); }
    )*};
}
case_tests! {
    layout_metadata_facts_are_real_and_read_only => "facts",
    layout_metadata_generation_bounds_and_digest => "generation",
    layout_metadata_scope_revision_handoff_and_active_transaction => "scope",
    layout_metadata_lease_near_expiry_and_expired => "lease",
    layout_metadata_unknown_schema_and_sidecars => "schema",
    layout_metadata_extension_and_runtime_rejection => "runtime",
    layout_metadata_real_page_cap_and_oversized_value_roll_back => "limits",
    layout_metadata_settings_restore_and_exclude_tag_policy => "restoration",
    layout_metadata_nonquiescent_finish_poison_survives_handoff => "poison",
    layout_metadata_failed_setting_restoration_poison => "restore-failure",
    layout_metadata_deadline_never_extends_and_stops_entered_work => "deadline",
    layout_metadata_retained_root_replacement_rejects => "replacement",
    layout_metadata_oversized_actual_database_rejects => "oversized-db",
    layout_metadata_unknown_collation_rejects => "collation",
    layout_metadata_real_hard_wal_pressure_rejects => "hard-wal",
}
#[test]
#[ignore = "bounded subprocess driver invoked by fifteen actual owner regressions"]
fn layout_metadata_child() {
    let Ok(case) = std::env::var("YIELD_LAYOUT_METADATA_CASE") else {
        return;
    };
    match case.as_str() {
        "facts" => facts(),
        "generation" => generation(),
        "scope" => scope(),
        "lease" => lease(),
        "schema" => schema(),
        "runtime" => runtime(),
        "limits" => limits(),
        "restoration" => restoration(),
        "poison" => poison(),
        "restore-failure" => restore_failure(),
        "deadline" => deadline(),
        "replacement" => replacement(),
        "oversized-db" => oversized_db(),
        "collation" => collation(),
        "hard-wal" => hard_wal(),
        _ => panic!("unknown layout case"),
    }
}
fn settings(writer: &SourceWriter) -> Vec<u64> {
    [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
    ]
    .map(|name| {
        writer
            .database_for_tests()
            .connection
            .pragma_query_value(None, name, |r| r.get(0))
            .unwrap()
    })
    .to_vec()
}
fn unchanged_rejection(writer: &SourceWriter, revision: &crate::sample_sources::SourceRevision) {
    let state = source_writer_state_snapshot(writer.database_for_tests());
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(writer.capture_layout_metadata_budget(revision).is_err());
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        state
    );
}
fn facts() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    connection.pragma_update(None, "query_only", true).unwrap();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let changes = connection.total_changes();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    assert_eq!(budget.revision(), &revision);
    assert_eq!(
        budget.page_size() * budget.maximum_pages(),
        64 * 1024 * 1024
    );
    assert!(budget.page_count() > 0 && budget.resident_database_bytes() > 0);
    assert!(budget.resident_shm_bytes() >= 32768);
    assert!(budget.required_metadata_bytes() > 128 * 1024 * 1024);
    assert_eq!(budget.existing_generation_bytes(), 0);
    assert_eq!(budget.existing_generation_digest(), None);
    assert_eq!(budget.maximum_generation_bytes(), 524288);
    assert_eq!(budget.maximum_generation_custody_bytes(), 524288);
    writer.validate_layout_metadata_budget(&budget).unwrap();
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
    assert!(writer.begin_layout_metadata_policy(&budget).is_err());
}
fn generation() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    // Bounded arbitrary bytes are observed, never adopted as a canonical/ready layout.
    let first = "untrusted-old-evidence";
    db.connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES (?1,?2)",
            rusqlite::params![KEY, first],
        )
        .unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    assert_eq!(budget.existing_generation_bytes(), first.len() as u64);
    assert_eq!(
        budget.existing_generation_digest(),
        Some(*blake3::hash(first.as_bytes()).as_bytes())
    );
    assert_eq!(
        budget.maximum_generation_custody_bytes(),
        524288 + first.len() as u64
    );
    db.connection
        .execute(
            "UPDATE metadata SET value=?1 WHERE key=?2",
            rusqlite::params!["changed-old-evidence!!", KEY],
        )
        .unwrap();
    assert!(writer.validate_layout_metadata_budget(&budget).is_err());
    for value in ["x".repeat(524288), "x".repeat(524289), String::new()] {
        db.connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key=?2",
                rusqlite::params![value, KEY],
            )
            .unwrap();
        if value.len() == 524288 {
            assert_eq!(
                writer
                    .capture_layout_metadata_budget(&revision)
                    .unwrap()
                    .existing_generation_bytes(),
                524288
            );
        } else {
            unchanged_rejection(&writer, &revision);
        }
    }
    for sql in [
        "UPDATE metadata SET value=CAST(x'80' AS TEXT) WHERE key=?1",
        "UPDATE metadata SET value=char(0) WHERE key=?1",
    ] {
        db.connection.execute(sql, [KEY]).unwrap();
        unchanged_rejection(&writer, &revision);
    }
}
fn scope() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    writer
        .database_for_tests()
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(writer.capture_layout_metadata_budget(&revision).is_err());
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert!(!writer.database_for_tests().connection.is_autocommit());
    writer
        .database_for_tests()
        .connection
        .execute_batch("ROLLBACK")
        .unwrap();
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
    assert!(matches!(
        writer.validate_layout_metadata_budget(&budget),
        Err(SourceWriterError::StaleFence)
    ));
    let current = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&current).unwrap();
    let fresh = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("layout-policy"),
        1,
    )
    .unwrap();
    assert!(matches!(
        fresh.validate_layout_metadata_budget(&budget),
        Err(SourceWriterError::StaleFence)
    ));
    fresh
        .capture_layout_metadata_budget(&fresh.current_revision().unwrap())
        .unwrap();
}
fn lease() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    db.connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 45],
        )
        .unwrap();
    let expires = now + 45;
    let changes = db.connection.total_changes();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let original = settings(&writer);
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    policy.validate_entered().unwrap();
    let mut batch = db.write_batch().unwrap();
    batch
        .set_metadata("layout-policy-lease-test", "runtime-only")
        .unwrap();
    policy.validate_entered().unwrap();
    batch.commit_auxiliary_state().unwrap();
    policy.finish().unwrap();
    assert_eq!(settings(&writer), original);
    let observed: i64 = db
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(observed, expires, "no separate control-writer renewal");
    assert_eq!(db.connection.total_changes(), changes + 1);
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    db.connection
        .execute_batch("UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1")
        .unwrap();
    unchanged_rejection(&writer, &revision);
    assert!(writer.begin_layout_metadata_policy(&budget).is_err());
}
fn schema() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    for (install, remove) in [
        (
            "CREATE TABLE unknown_layout_table(x TEXT)",
            "DROP TABLE unknown_layout_table",
        ),
        (
            "CREATE TEMP TABLE unknown_layout_temp(x TEXT)",
            "DROP TABLE unknown_layout_temp",
        ),
        (
            "CREATE TRIGGER unknown_layout_trigger AFTER INSERT ON metadata BEGIN SELECT 1; END",
            "DROP TRIGGER unknown_layout_trigger",
        ),
        (
            "ATTACH ':memory:' AS unknown_layout_attached",
            "DETACH unknown_layout_attached",
        ),
        (
            "PRAGMA locking_mode=EXCLUSIVE",
            "PRAGMA locking_mode=NORMAL",
        ),
    ] {
        writer
            .database_for_tests()
            .connection
            .execute_batch(install)
            .unwrap();
        unchanged_rejection(&writer, &revision);
        writer
            .database_for_tests()
            .connection
            .execute_batch(remove)
            .unwrap();
    }
    let path = writer.database_for_tests().db_path.with_file_name(format!(
        "{}-shm",
        writer
            .database_for_tests()
            .db_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    ));
    let old = path.with_extension("retained-layout-shm");
    std::fs::rename(&path, &old).unwrap();
    unchanged_rejection(&writer, &revision);
    std::fs::rename(&old, &path).unwrap();
    writer.capture_layout_metadata_budget(&revision).unwrap();
}
fn runtime() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let file = fixture._source.path().join("invalid-layout-extension");
    std::fs::write(&file, b"not a SQLite extension").unwrap();
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    assert!(
        crate::sqlite_ext::load_extension_for_test(&writer.database_for_tests().connection, &file)
            .is_err()
    );
    policy.validate_entered().unwrap();
    policy.finish().unwrap();
    // After release, a real attempted initialization taints this process permanently.
    assert!(
        crate::sqlite_ext::load_extension_for_test(&writer.database_for_tests().connection, &file)
            .is_err()
    );
    unchanged_rejection(&writer, &revision);
}
fn limits() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    let baseline = writer.capture_layout_metadata_budget(&revision).unwrap();
    let cap = baseline.page_count() + 2;
    db.connection
        .pragma_update(None, "max_page_count", cap)
        .unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    assert_eq!(budget.maximum_pages(), cap);
    assert!(budget.required_metadata_bytes() < baseline.required_metadata_bytes());
    let before = source_writer_state_snapshot(db);
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    let batch = db.write_batch().unwrap();
    let error = batch
        .curation_transaction()
        .execute(
            "INSERT INTO metadata(key,value) VALUES (?1,zeroblob(524288))",
            [KEY],
        )
        .unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(ref e, _) if e.code == rusqlite::ErrorCode::DiskFull)
    );
    drop(batch);
    policy.finish().unwrap();
    assert_eq!(source_writer_state_snapshot(db), before);
    assert_eq!(db.get_metadata(KEY).unwrap(), None);
    db.connection
        .pragma_update(None, "max_page_count", baseline.maximum_pages())
        .unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    let mut batch = db.write_batch().unwrap();
    batch.set_metadata(KEY, &"x".repeat(524289)).unwrap();
    assert!(matches!(
        policy.validate_entered(),
        Err(SourceWriterError::MetadataBoundNotDefined)
    ));
    drop(batch);
    policy.finish().unwrap();
    assert_eq!(db.get_metadata(KEY).unwrap(), None);
}
fn restoration() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let tag = writer.capture_tag_metadata_budget(&revision).unwrap();
    let original = settings(&writer);
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    assert_eq!(settings(&writer), vec![budget.maximum_pages(), 2, 0, 0]);
    assert!(writer.begin_tag_metadata_policy(&tag).is_err());
    assert!(writer.begin_layout_metadata_policy(&budget).is_err());
    assert!(writer.capture_layout_metadata_budget(&revision).is_err());
    let mut batch = writer.database_for_tests().write_batch().unwrap();
    batch
        .set_metadata("layout-runtime-fixture", "rollback")
        .unwrap();
    policy.validate_entered().unwrap();
    drop(batch);
    drop(policy);
    assert_eq!(settings(&writer), original);
    assert_eq!(
        writer
            .database_for_tests()
            .get_metadata("layout-runtime-fixture")
            .unwrap(),
        None
    );
    let tag = writer.capture_tag_metadata_budget(&revision).unwrap();
    let tag_policy = writer.begin_tag_metadata_policy(&tag).unwrap();
    assert!(writer.begin_layout_metadata_policy(&budget).is_err());
    tag_policy.finish().unwrap();
}
fn poison() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    let batch = writer.database_for_tests().write_batch().unwrap();
    assert!(matches!(
        policy.finish(),
        Err(SourceWriterError::Database(
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
    let fresh = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("layout-policy"),
        1,
    )
    .unwrap();
    unchanged_rejection(&fresh, &revision);
    assert!(matches!(
        fresh.database_for_tests().write_batch(),
        Err(SourceDbError::MetadataPolicyUnconfirmed)
    ));
}
fn restore_failure() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    unsafe extern "C" fn deny_temp_store(
        _: *mut std::ffi::c_void,
        action: i32,
        name: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == rusqlite::ffi::SQLITE_PRAGMA
            && !name.is_null()
            && unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes() == b"temp_store"
        {
            rusqlite::ffi::SQLITE_DENY
        } else {
            rusqlite::ffi::SQLITE_OK
        }
    }
    // SAFETY: static callback, exclusive fixture connection, removed before cleanup.
    unsafe {
        rusqlite::ffi::sqlite3_set_authorizer(
            writer.database_for_tests().connection.handle(),
            Some(deny_temp_store),
            std::ptr::null_mut(),
        );
    }
    assert!(matches!(
        policy.finish(),
        Err(SourceWriterError::Database(
            SourceDbError::MetadataPolicyUnconfirmed
        ))
    ));
    unsafe {
        rusqlite::ffi::sqlite3_set_authorizer(
            writer.database_for_tests().connection.handle(),
            None,
            std::ptr::null_mut(),
        );
    }
    assert!(matches!(
        writer.database_for_tests().write_batch(),
        Err(SourceDbError::MetadataPolicyUnconfirmed)
    ));
    unchanged_rejection(&writer, &revision);
}
fn deadline() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
    let policy = writer.begin_layout_metadata_policy(&budget).unwrap();
    std::thread::sleep(
        budget
            .observation_deadline()
            .saturating_duration_since(Instant::now())
            + Duration::from_millis(20),
    );
    assert!(matches!(
        policy.validate_entered(),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    policy.finish().unwrap();
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(matches!(
        writer.validate_layout_metadata_budget(&budget),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    writer.capture_layout_metadata_budget(&revision).unwrap();
}
fn replacement() {
    #[cfg(unix)]
    {
        owned!(fixture, writer);
        let revision = writer.current_revision().unwrap();
        let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
        let child = fixture
            .profile_guard
            .profile_root()
            .join(fixture.profile_child.as_ref().unwrap());
        let old = child.with_extension("retained-layout-root");
        std::fs::rename(&child, &old).unwrap();
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("sentinel"), b"unchanged").unwrap();
        assert!(writer.validate_layout_metadata_budget(&budget).is_err());
        assert!(writer.begin_layout_metadata_policy(&budget).is_err());
        assert_eq!(std::fs::read(child.join("sentinel")).unwrap(), b"unchanged");
        std::fs::remove_dir_all(&child).unwrap();
        std::fs::rename(old, &child).unwrap();
    }
}

fn oversized_db() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    db.connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('layout-db-size-fixture',zeroblob(68157440))",
            [],
        )
        .unwrap();
    db.connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert!(crate::sqlite_wal::retained_database_bytes(&db.connection).unwrap() > 64 * 1024 * 1024);
    unchanged_rejection(&writer, &revision);
}
fn collation() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    unsafe extern "C" fn compare(
        _: *mut std::ffi::c_void,
        _: i32,
        _: *const std::ffi::c_void,
        _: i32,
        _: *const std::ffi::c_void,
    ) -> i32 {
        0
    }
    // SAFETY: static callback/name, exclusive fixture connection, reclaimed by SQLite.
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_create_collation_v2(
                writer.database_for_tests().connection.handle(),
                c"unknown_layout_callback".as_ptr(),
                rusqlite::ffi::SQLITE_UTF8,
                std::ptr::null_mut(),
                Some(compare),
                None,
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    unchanged_rejection(&writer, &revision);
}
fn hard_wal() {
    owned!(fixture, writer);
    let revision = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    db.connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let hard = crate::sqlite_wal::WAL_HARD_WATERMARK_BYTES;
    for _ in 0..70 {
        if crate::sqlite_wal::retained_wal_bytes(&db.connection).unwrap() >= hard {
            break;
        }
        db.connection.execute("INSERT OR REPLACE INTO metadata(key,value) VALUES ('layout-wal-pressure',zeroblob(1048576))", []).unwrap();
    }
    assert!(crate::sqlite_wal::retained_wal_bytes(&db.connection).unwrap() >= hard);
    let error = writer
        .capture_layout_metadata_budget(&revision)
        .unwrap_err();
    assert!(
        error.to_string().contains("WalHardWatermark"),
        "unexpected error: {error}"
    );
    unchanged_rejection(&writer, &revision);
}
