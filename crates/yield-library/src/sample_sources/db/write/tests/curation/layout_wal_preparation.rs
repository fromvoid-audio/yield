//! Real owned layout policy/preparation; no admitted publication or physical qualification.
use super::tag_wal_preparation::{
    assert_busy, authorizer, control_changes, db, deny, deny_begin, deny_restore, expiry, external,
    request, settings, writer,
};
use super::*;
use crate::sample_sources::db::{SourceLayoutMetadataBudget, write::SourceWalPreparationError};
use rusqlite::{ErrorCode, ffi};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
fn observe(writer: &SourceWriter) -> SourceLayoutMetadataBudget {
    writer
        .capture_layout_metadata_budget(&writer.current_revision().unwrap())
        .unwrap()
}
macro_rules! case {
    ($name:ident,$case:literal) => {
        #[test]
        fn $name() {
            tag_metadata_budget::run_case($case);
        }
    };
}
case!(
    layout_owned_preparation_restores_exact_connection_without_nested_begin,
    "layout-owned-success"
);
case!(
    layout_owned_preparation_original_deadline_cannot_be_extended,
    "layout-owned-deadline"
);
case!(
    layout_owned_preparation_wrong_issuer_and_revision_are_rejected,
    "layout-owned-issuer"
);
case!(
    layout_owned_preparation_rejects_changed_generation_bytes_and_schema,
    "layout-owned-scope"
);
case!(
    layout_owned_preparation_never_renews_near_or_expired_lease,
    "layout-owned-lease"
);
case!(
    layout_owned_preparation_busy_attempt_restores_without_waiting,
    "layout-owned-busy"
);
case!(
    layout_owned_preparation_retains_owner_until_real_rollback_succeeds,
    "layout-owned-rollback"
);
case!(
    layout_owned_preparation_failed_restoration_is_sticky_until_close,
    "layout-owned-restore"
);
case!(
    layout_owned_preparation_partial_entry_restores_original_settings,
    "layout-owned-partial"
);
case!(
    layout_owned_preparation_drop_releases_without_mutation,
    "layout-owned-dropped"
);
case!(
    layout_owned_preparation_rejects_hard_and_unknown_wal,
    "layout-owned-wal"
);
case!(
    layout_owned_preparation_rejects_root_main_profile_substitution,
    "layout-owned-bindings"
);
case!(
    layout_owned_preparation_never_adopts_or_returns_preexisting_transaction,
    "layout-owned-active"
);
case!(
    layout_owned_preparation_measures_exact_bounded_blob_bytes_without_semantic_adoption,
    "layout-owned-blob"
);
case!(
    layout_owned_preparation_measures_actual_prepared_canonical_blob_without_mutation,
    "layout-owned-canonical"
);
pub(super) fn dispatch(case: &str) {
    match case {
        "layout-owned-success" => success(),
        "layout-owned-deadline" => deadline(),
        "layout-owned-issuer" => issuer(),
        "layout-owned-scope" => scope(),
        "layout-owned-lease" => lease(),
        "layout-owned-busy" => busy(),
        "layout-owned-rollback" => rollback(),
        "layout-owned-restore" => restore(),
        "layout-owned-partial" => partial(),
        "layout-owned-dropped" => dropped(),
        "layout-owned-wal" => wal(),
        "layout-owned-bindings" => bindings(),
        "layout-owned-active" => active(),
        "layout-owned-blob" => blob(),
        "layout-owned-canonical" => canonical(),
        _ => panic!("unknown layout-owned case: {case}"),
    }
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
    let revision = obs.revision().clone();
    let held = writer
        .prepare_layout_wal_reservation(obs, request())
        .unwrap();
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
    let held = writer
        .prepare_layout_wal_reservation(obs, request())
        .unwrap();
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
        .prepare_layout_wal_reservation(obs, request())
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
                    obs.revision(),
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
            .prepare_layout_wal_reservation(obs, request())
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
    {
        let (writer, _fixture, _) = writer();
        db(&writer)
            .execute(
                "INSERT INTO metadata(key,value) VALUES ('source_starmap_generation_v1',?1)",
                [b"X\0\0\0".as_slice()],
            )
            .unwrap();
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let original = settings(&writer);
        let handle = unsafe { db(&writer).handle() };
        let obs = observe(&writer);
        let held = writer
            .prepare_layout_wal_reservation(obs, request())
            .unwrap();
        // Real owner fault within the held transaction, with unchanged revision and row size.
        assert_eq!(
            unsafe {
                ffi::sqlite3_exec(handle,c"UPDATE metadata SET value=x'59000000' WHERE key='source_starmap_generation_v1'".as_ptr(),None,std::ptr::null_mut(),std::ptr::null_mut())
            },
            ffi::SQLITE_OK
        );
        authorizer(handle, Some(deny_begin));
        assert!(matches!(
            held.revalidate(),
            Err(SourceWalPreparationError::Owner(
                SourceWriterError::MetadataBoundNotDefined
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

    for sql in [
        "UPDATE metadata SET value=x'59000000' WHERE key='source_starmap_generation_v1'",
        "CREATE TABLE unknown_layout_owned(value TEXT)",
    ] {
        let (writer, _fixture, _) = writer();
        db(&writer)
            .execute(
                "INSERT INTO metadata(key,value) VALUES ('source_starmap_generation_v1',?1)",
                [b"X\0\0\0".as_slice()],
            )
            .unwrap();
        db(&writer)
            .execute_batch("PRAGMA wal_checkpoint(RESTART)")
            .unwrap();
        let obs = observe(&writer);
        db(&writer).execute_batch(sql).unwrap();
        let changes = db(&writer).total_changes();
        let original = settings(&writer);
        let failure = writer
            .prepare_layout_wal_reservation(obs, request())
            .unwrap_err();
        let writer = failure.into_writer().unwrap();
        assert_eq!(db(&writer).total_changes(), changes);
        assert_eq!(settings(&writer), original);
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
        let result = writer.prepare_layout_wal_reservation(obs, request());
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
        .prepare_layout_wal_reservation(obs, request())
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
        .prepare_layout_wal_reservation(obs, request())
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
    let held = writer
        .prepare_layout_wal_reservation(obs, request())
        .unwrap();
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
    let held = writer
        .prepare_layout_wal_reservation(obs, request())
        .unwrap();
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
        .prepare_layout_wal_reservation(obs, request())
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
    let held = writer
        .prepare_layout_wal_reservation(obs, request())
        .unwrap();
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
            .prepare_layout_wal_reservation(obs, request())
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
            .prepare_layout_wal_reservation(obs, request())
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
        let held = writer
            .prepare_layout_wal_reservation(obs, request())
            .unwrap();
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
fn active() {
    let (writer, fixture, _) = writer();
    let other = external(&writer);
    let obs = observe(&writer);
    db(&writer).execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = writer
        .prepare_layout_wal_reservation(obs, request())
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

fn blob() {
    let (writer, _fixture, _) = writer();
    let rev = writer.current_revision().unwrap();
    for len in [1, 524288, 524289, 0] {
        let value = vec![0; len];
        db(&writer).execute("INSERT OR REPLACE INTO metadata(key,value) VALUES ('source_starmap_generation_v1',?1)",[&value]).unwrap();
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let changes = db(&writer).total_changes();
        let result = writer.capture_layout_metadata_budget(&rev);
        if len > 0 && len <= 524288 {
            let budget = result.unwrap();
            assert_eq!(budget.existing_generation_bytes(), len as u64);
            assert_eq!(
                budget.existing_generation_digest(),
                Some(*blake3::hash(&value).as_bytes())
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(db(&writer).total_changes(), changes);
        assert_eq!(
            source_writer_state_snapshot(writer.database_for_tests()),
            before
        );
    }
}
fn canonical() {
    super::layout_capture::canonical_blob_budget();
}
