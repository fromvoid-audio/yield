//! Real profile SQLite owners; neither these preparations nor their tests grant admission.
use super::*;
use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
use crate::sample_sources::db::SourceTagAdmissionObservation;
use crate::sample_sources::db::write::SourceWalPreparationError;
use rusqlite::{Connection, ErrorCode, ffi};
use std::{
    ffi::{CStr, c_char, c_void},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

macro_rules! case {
    ($name:ident, $case:literal) => {
        #[test]
        fn $name() {
            tag_metadata_budget::run_case($case);
        }
    };
}
case!(
    tag_owned_preparation_restores_exact_connection_without_nested_begin,
    "tag-owned-success"
);
case!(
    tag_owned_preparation_original_deadline_cannot_be_extended,
    "tag-owned-deadline"
);
case!(
    tag_owned_preparation_wrong_issuer_and_revision_are_rejected,
    "tag-owned-issuer"
);
case!(
    tag_owned_preparation_rejects_full_scope_and_directory_changes,
    "tag-owned-scope"
);
case!(
    tag_owned_preparation_never_renews_near_or_expired_lease,
    "tag-owned-lease"
);
case!(
    tag_owned_preparation_busy_attempt_restores_without_waiting,
    "tag-owned-busy"
);
case!(
    tag_owned_preparation_retains_owner_until_real_rollback_succeeds,
    "tag-owned-rollback"
);
case!(
    tag_owned_preparation_failed_restoration_is_sticky_until_close,
    "tag-owned-restore"
);
case!(
    tag_owned_preparation_partial_entry_restores_original_settings,
    "tag-owned-partial"
);
case!(
    tag_owned_preparation_drop_releases_reservation_without_mutation,
    "tag-owned-drop"
);

pub(super) fn dispatch(case: &str) {
    match case {
        "tag-owned-success" => success(),
        "tag-owned-deadline" => deadline(),
        "tag-owned-issuer" => issuer(),
        "tag-owned-scope" => scope(),
        "tag-owned-lease" => lease(),
        "tag-owned-busy" => busy(),
        "tag-owned-rollback" => rollback(),
        "tag-owned-restore" => restore(),
        "tag-owned-partial" => partial(),
        "tag-owned-drop" => dropped(),
        "tag-owned-active" => active(),
        "tag-owned-bounds" => bounds(),
        "tag-owned-wal" => wal(),
        "tag-owned-bindings" => bindings(),
        _ => panic!("unknown tag-owned fixture: {case}"),
    }
}
fn writer() -> (SourceWriter, CurationFixture, i64) {
    let mut fixture = CurationFixture::new_profile_bound();
    let old = insert_tag(&fixture.database, "old tag", "Old Tag");
    fixture.revision = fixture
        .commit(&[
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: old,
            },
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("two.wav"),
                tag_id: old,
            },
        ])
        .unwrap()
        .source_revision();
    // Keep a genuine read-only fixture observer, not another writable owner.
    let file: String = fixture
        .database
        .connection
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let observer = SourceDatabase::open_for_ui_read_with_database_root(
        fixture._source.path(),
        Path::new(&file).parent().unwrap(),
    )
    .unwrap();
    let database = std::mem::replace(&mut fixture.database, observer);
    let writer = SourceWriter::new(database, SourceId::from_string("tag-owned"), 7).unwrap();
    (writer, fixture, old)
}
fn observe(writer: &SourceWriter) -> SourceTagAdmissionObservation {
    writer
        .capture_tag_admission_observation(
            &writer.current_revision().unwrap(),
            "old tag",
            Mutation::Delete,
        )
        .unwrap()
}
fn request() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn db(writer: &SourceWriter) -> &Connection {
    &writer.database_for_tests().connection
}
fn settings(writer: &SourceWriter) -> Vec<u64> {
    [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
        "busy_timeout",
    ]
    .map(|name| {
        db(writer)
            .pragma_query_value(None, name, |r| r.get(0))
            .unwrap()
    })
    .to_vec()
}
fn external(writer: &SourceWriter) -> Connection {
    let c = Connection::open_with_flags(
        &writer.database_for_tests().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    c.busy_timeout(Duration::ZERO).unwrap();
    c
}
fn assert_busy(c: &Connection) {
    assert_eq!(
        c.execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
}
fn control_changes(writer: &SourceWriter) -> u64 {
    writer
        .database_for_tests()
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap()
}
fn expiry(c: &Connection) -> i64 {
    c.query_row(
        "SELECT expires_at FROM source_lease WHERE singleton=1",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

// Static callbacks only; SQLite executes them synchronously on the retained owner's thread.
unsafe extern "C" fn deny(
    _: *mut c_void,
    action: i32,
    a: *const c_char,
    b: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> i32 {
    let a = if a.is_null() {
        b"".as_slice()
    } else {
        unsafe { CStr::from_ptr(a) }.to_bytes()
    };
    if (action == ffi::SQLITE_TRANSACTION && a == b"ROLLBACK")
        || (action == ffi::SQLITE_PRAGMA && a == b"temp_store" && !b.is_null())
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn deny_begin(
    _: *mut c_void,
    action: i32,
    a: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> i32 {
    if action == ffi::SQLITE_TRANSACTION
        && !a.is_null()
        && unsafe { CStr::from_ptr(a) }.to_bytes() == b"BEGIN"
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn deny_restore(
    _: *mut c_void,
    action: i32,
    a: *const c_char,
    b: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> i32 {
    if action == ffi::SQLITE_PRAGMA
        && !a.is_null()
        && unsafe { CStr::from_ptr(a) }.to_bytes() == b"temp_store"
        && !b.is_null()
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}
fn authorizer(
    handle: *mut ffi::sqlite3,
    callback: Option<
        unsafe extern "C" fn(
            *mut c_void,
            i32,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> i32,
    >,
) {
    // SAFETY: exact live owner handle, same thread, static callback, no userdata.
    assert_eq!(
        unsafe { ffi::sqlite3_set_authorizer(handle, callback, std::ptr::null_mut()) },
        ffi::SQLITE_OK
    );
}
fn success() {
    let (writer, fixture, _) = writer();
    db(&writer)
        .busy_timeout(Duration::from_millis(913))
        .unwrap();
    let original = settings(&writer);
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let changes = db(&writer).total_changes();
    let lease_before = expiry(db(&writer));
    let control_before = control_changes(&writer);
    let other = external(&writer);
    let handle = unsafe { db(&writer).handle() };
    let obs = observe(&writer);
    let revision = obs.selection().revision().clone();
    let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
    assert_busy(&other);
    authorizer(handle, Some(deny_begin));
    held.revalidate().unwrap();
    authorizer(handle, None);
    let writer = held.abort().unwrap();
    assert_eq!(unsafe { db(&writer).handle() }, handle);
    assert_eq!(settings(&writer), original);
    assert_eq!(expiry(db(&writer)), lease_before);
    assert_eq!(control_changes(&writer), control_before);
    assert_eq!(db(&writer).total_changes(), changes);
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
    assert_eq!(writer.current_revision().unwrap(), revision);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    writer
        .commit_catalog_curation_command(
            &revision,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            }],
        )
        .unwrap();
}
fn deadline() {
    let (writer, _fixture, _) = writer();
    let other = external(&writer);
    let original = settings(&writer);
    let obs = observe(&writer);
    std::thread::sleep(Duration::from_millis(1700));
    let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
    std::thread::sleep(Duration::from_millis(350));
    assert!(held.is_expired());
    assert!(matches!(
        held.revalidate(),
        Err(SourceWalPreparationError::Deadline)
    ));
    assert_busy(&other);
    let writer = held.abort().unwrap();
    assert_eq!(settings(&writer), original);
    let obs = observe(&writer);
    std::thread::sleep(Duration::from_millis(2050));
    let failure = writer
        .prepare_tag_wal_reservation(obs, request())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::Deadline
    ));
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
}
fn issuer() {
    for changed_revision in [false, true] {
        let (writer, fixture, _) = writer();
        let obs = observe(&writer);
        let writer = if changed_revision {
            writer
                .commit_catalog_curation_command(
                    obs.selection().revision(),
                    &fixture.profile_guard,
                    &[SourceCurationUpdate::SetLocked {
                        path: CurationFixture::path("one.wav"),
                        locked: true,
                    }],
                )
                .unwrap();
            writer
        } else {
            SourceWriter::new(
                writer.into_catalog_database(),
                SourceId::from_string("tag-owned"),
                7,
            )
            .unwrap()
        };
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let original = settings(&writer);
        let failure = writer
            .prepare_tag_wal_reservation(obs, request())
            .unwrap_err();
        let writer = failure.into_writer().unwrap();
        assert_eq!(settings(&writer), original);
        assert_eq!(
            source_writer_state_snapshot(writer.database_for_tests()),
            before
        );
    }
}
fn scope() {
    for sql in [
        "UPDATE source_directory_generations SET complete=0 WHERE status='active'",
        "DELETE FROM wav_file_tags WHERE path='two.wav'",
        "UPDATE source_directory_generations SET generation=2 WHERE status='active'",
        "UPDATE source_directory_generations SET published_source_revision=999 WHERE status='active'",
        "CREATE TABLE unknown_tag_owned(value TEXT)",
    ] {
        let (writer, _fixture, _) = writer();
        db(&writer)
            .execute_batch("PRAGMA wal_checkpoint(RESTART)")
            .unwrap();
        let obs = observe(&writer);
        db(&writer)
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db(&writer).execute_batch(sql).unwrap();
        db(&writer)
            .execute_batch("PRAGMA ignore_check_constraints=OFF")
            .unwrap();
        let original = settings(&writer);
        let changes = db(&writer).total_changes();
        let failure = writer
            .prepare_tag_wal_reservation(obs, request())
            .unwrap_err();
        let writer = failure.into_writer().unwrap();
        assert_eq!(settings(&writer), original);
        assert_eq!(db(&writer).total_changes(), changes);
    }
}
fn lease() {
    for offset in [1, -1, 45] {
        let (writer, _fixture, _) = writer();
        let obs = observe(&writer);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db(&writer)
            .execute(
                "UPDATE source_lease SET acquired_at=?1, expires_at=?2 WHERE singleton=1",
                rusqlite::params![now - 2, now + offset],
            )
            .unwrap();
        let lease_before = expiry(db(&writer));
        let control_before = control_changes(&writer);
        let changes = db(&writer).total_changes();
        let obs = if offset == 45 { observe(&writer) } else { obs };
        let result = writer.prepare_tag_wal_reservation(obs, request());
        let writer = if offset == 45 {
            result.unwrap().abort().unwrap()
        } else {
            result.unwrap_err().into_writer().unwrap()
        };
        assert_eq!(expiry(db(&writer)), lease_before);
        assert_eq!(control_changes(&writer), control_before);
        assert_eq!(db(&writer).total_changes(), changes);
    }
}
fn busy() {
    let (writer, _fixture, _) = writer();
    let obs = observe(&writer);
    db(&writer).busy_timeout(Duration::from_secs(9)).unwrap();
    let original = settings(&writer);
    let other = external(&writer);
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    let failure = writer
        .prepare_tag_wal_reservation(obs, request())
        .unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        matches!(failure.error(),SourceWalPreparationError::Sqlite(e) if e.sqlite_error_code()==Some(ErrorCode::DatabaseBusy))
    );
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    other.execute_batch("ROLLBACK").unwrap();
    let obs = observe(&writer);
    writer
        .prepare_tag_wal_reservation(obs, request())
        .unwrap()
        .abort()
        .unwrap();
}
fn rollback() {
    let (writer, _fixture, _) = writer();
    let original = settings(&writer);
    let other = external(&writer);
    let handle = unsafe { db(&writer).handle() };
    let obs = observe(&writer);
    let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
    authorizer(handle, Some(deny));
    let failure = held.abort().err().expect("cleanup must fail");
    assert_busy(&other);
    // A denied rollback must not try policy restoration; removing the fault recovers the same owner.
    authorizer(handle, None);
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    assert_eq!(unsafe { db(&writer).handle() }, handle);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}
fn restore() {
    let (writer, fixture, _) = writer();
    let other = external(&writer);
    let handle = unsafe { db(&writer).handle() };
    let obs = observe(&writer);
    let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
    authorizer(handle, Some(deny_restore));
    let failure = held.abort().err().expect("cleanup must fail");
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    authorizer(handle, None);
    let failure = failure.into_writer().err().expect("poison is sticky");
    drop(failure);
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let fresh = SourceWriter::new(database, SourceId::from_string("tag-owned"), 7).unwrap();
    observe(&fresh);
}
fn partial() {
    let (writer, _fixture, _) = writer();
    let original = settings(&writer);
    let handle = unsafe { db(&writer).handle() };
    let obs = observe(&writer);
    authorizer(handle, Some(deny_restore));
    let failure = writer
        .prepare_tag_wal_reservation(obs, request())
        .unwrap_err();
    authorizer(handle, None);
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    observe(&writer);
}
fn dropped() {
    let (writer, fixture, _) = writer();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let other = external(&writer);
    let obs = observe(&writer);
    let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
    assert_busy(&other);
    drop(held);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(source_writer_state_snapshot(&fixture.database), before);
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let fresh = SourceWriter::new(database, SourceId::from_string("tag-owned"), 7).unwrap();
    observe(&fresh);
}

case!(
    tag_owned_preparation_checks_last_assignment_in_64_rename_and_128_delete,
    "tag-owned-bounds"
);
case!(
    tag_owned_preparation_rejects_retained_hard_and_unknown_wal,
    "tag-owned-wal"
);
case!(
    tag_owned_preparation_rejects_root_main_and_profile_substitution,
    "tag-owned-bindings"
);

fn bounds() {
    for count in [64, 128] {
        let (writer, _fixture, old) = writer();
        db(&writer)
            .execute("DELETE FROM wav_file_tags WHERE tag_id=?1", [old])
            .unwrap();
        for i in (0..count).rev() {
            let path = format!("{i:03}.wav");
            db(&writer).execute("INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES (?1,1,1,'wav')",[&path]).unwrap();
            db(&writer)
                .execute(
                    "INSERT INTO wav_file_tags(path,tag_id) VALUES (?1,?2)",
                    rusqlite::params![path, old],
                )
                .unwrap();
        }
        let mutation = if count == 64 {
            Mutation::Rename("new tag".into())
        } else {
            Mutation::Delete
        };
        let obs = writer
            .capture_tag_admission_observation(
                &writer.current_revision().unwrap(),
                "old tag",
                mutation,
            )
            .unwrap();
        assert_eq!(obs.selection().paths().len(), count);
        assert!(
            obs.selection()
                .paths()
                .windows(2)
                .all(|p| p[0].as_path() < p[1].as_path())
        );
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let original = settings(&writer);
        let handle = unsafe { db(&writer).handle() };
        let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
        held.revalidate().unwrap();
        // Test fault on the actual retained connection after preparation: only the last item
        // changes. SQLite rolls it back with the otherwise empty preparation on abort.
        let sql = std::ffi::CString::new(format!(
            "DELETE FROM wav_file_tags WHERE path='{:03}.wav'",
            count - 1
        ))
        .unwrap();
        assert_eq!(
            unsafe {
                ffi::sqlite3_exec(
                    handle,
                    sql.as_ptr(),
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            ffi::SQLITE_OK
        );
        authorizer(handle, Some(deny_begin));
        assert!(matches!(
            held.revalidate(),
            Err(SourceWalPreparationError::Owner(
                SourceWriterError::StaleFence
            ))
        ));
        authorizer(handle, None);
        let writer = held.abort().unwrap();
        assert_eq!(settings(&writer), original);
        assert_eq!(
            source_writer_state_snapshot(writer.database_for_tests()),
            before
        );
    }
}
fn wal() {
    {
        let (writer, _fixture, _) = writer();
        let obs = observe(&writer);
        let original = settings(&writer);
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                ffi::sqlite3_file_control(
                    db(&writer).handle(),
                    c"main".as_ptr(),
                    ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                    (&mut file as *mut *mut ffi::sqlite3_file).cast(),
                )
            },
            ffi::SQLITE_OK
        );
        assert!(!file.is_null());
        let methods = unsafe { (*file).pMethods };
        assert!(!methods.is_null());
        let mut copy = Box::new(unsafe { methods.read() });
        unsafe extern "C" fn unknown(_: *mut ffi::sqlite3_file, _: *mut ffi::sqlite3_int64) -> i32 {
            ffi::SQLITE_IOERR_FSTAT
        }
        copy.xFileSize = Some(unknown);
        // Per-file methods only; the genuine owner remains retained through the failure.
        unsafe { (*file).pMethods = &*copy };
        let failure = writer
            .prepare_tag_wal_reservation(obs, request())
            .unwrap_err();
        unsafe { (*file).pMethods = methods };
        drop(copy);
        let writer = failure.into_writer().unwrap();
        assert_eq!(settings(&writer), original);
        assert_eq!(
            source_writer_state_snapshot(writer.database_for_tests()),
            before
        );
    }
    {
        let (writer, _fixture, _) = writer();
        let obs = observe(&writer);
        db(&writer)
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        let reader = external(&writer);
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM metadata")
            .unwrap();
        db(&writer)
            .execute(
                "INSERT INTO metadata(key,value) VALUES ('tag-owned-pressure',zeroblob(1048576))",
                [],
            )
            .unwrap();
        for _ in 0..65 {
            db(&writer)
                .execute(
                    "UPDATE metadata SET value=randomblob(1048576) WHERE key='tag-owned-pressure'",
                    [],
                )
                .unwrap();
        }
        assert!(matches!(
            crate::sqlite_wal::enforce_wal_admission(db(&writer)),
            Err(crate::WalAdmissionError::HardWatermark { .. })
        ));
        let original = settings(&writer);
        let changes = db(&writer).total_changes();
        let failure = writer
            .prepare_tag_wal_reservation(obs, request())
            .unwrap_err();
        let writer = failure.into_writer().unwrap();
        assert_eq!(settings(&writer), original);
        assert_eq!(db(&writer).total_changes(), changes);
        reader.execute_batch("ROLLBACK").unwrap();
    }
}
fn bindings() {
    for kind in ["root", "main", "profile"] {
        let (writer, fixture, _) = writer();
        let original = settings(&writer);
        let other = external(&writer);
        let handle = unsafe { db(&writer).handle() };
        let path = match kind {
            "root" => fixture._source.path().to_path_buf(),
            "main" => writer.database_for_tests().db_path.clone(),
            _ => fixture.profile_guard.profile_root().to_path_buf(),
        };
        let saved = path.with_extension("tag-owned-saved");
        let obs = observe(&writer);
        let held = writer.prepare_tag_wal_reservation(obs, request()).unwrap();
        std::fs::rename(&path, &saved).unwrap();
        assert!(held.revalidate().is_err());
        if kind == "main" {
            std::fs::write(&path, b"replacement").unwrap();
        } else {
            std::fs::create_dir(&path).unwrap();
        }
        assert!(held.revalidate().is_err());
        assert_eq!(unsafe { ffi::sqlite3_get_autocommit(handle) }, 0);
        if kind == "main" {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::remove_dir(&path).unwrap();
        }
        std::fs::rename(saved, &path).unwrap();
        assert_busy(&other);
        held.revalidate().unwrap();
        let writer = held.abort().unwrap();
        assert_eq!(settings(&writer), original);
    }
}

case!(
    tag_owned_preparation_never_returns_or_adopts_a_preexisting_transaction,
    "tag-owned-active"
);
fn active() {
    let (writer, fixture, _) = writer();
    let other = external(&writer);
    let obs = observe(&writer);
    db(&writer).execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = writer
        .prepare_tag_wal_reservation(obs, request())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::TransactionActive
    ));
    let failure = failure
        .into_writer()
        .err()
        .expect("active owner must remain inaccessible");
    assert_busy(&other);
    drop(failure);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let fresh = SourceWriter::new(database, SourceId::from_string("tag-owned"), 7).unwrap();
    observe(&fresh);
}
