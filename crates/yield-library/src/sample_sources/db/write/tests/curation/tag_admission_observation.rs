use super::*;
use crate::sample_sources::SourceTagAssignmentMutation as Mutation;

#[test]
fn tag_admission_observation_captures_complete_measured_scope_without_writes() {
    tag_metadata_budget::run_case("tag-observation-capture");
}
#[test]
fn tag_admission_observation_rejects_changed_generation_membership_schema_and_budget() {
    tag_metadata_budget::run_case("tag-observation-scope");
}
#[test]
fn tag_admission_observation_rejects_owner_handoff_and_changed_revision() {
    tag_metadata_budget::run_case("tag-observation-owner");
}

fn prepared() -> (CurationFixture, i64) {
    let mut fixture = CurationFixture::new_profile_bound();
    let old = insert_tag(&fixture.database, "old tag", "Old Tag");
    let receipt = fixture
        .commit(&[
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("two.wav"),
                tag_id: old,
            },
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: old,
            },
        ])
        .unwrap();
    fixture.revision = receipt.source_revision();
    (fixture, old)
}

pub(super) fn capture() {
    let (fixture, old) = prepared();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("tag-observation"),
        7,
    )
    .unwrap();
    let revision = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let changes = writer.database_for_tests().connection.total_changes();
    let observed = writer
        .capture_tag_admission_observation(
            &revision,
            "  OLD   tag ",
            Mutation::Rename(" NEW tag ".into()),
        )
        .unwrap();
    assert_eq!(observed.directory_generation(), 1);
    assert_eq!(observed.selection().old_tag_id(), old);
    assert_eq!(observed.selection().old_identity(), "old tag");
    assert_eq!(
        observed.selection().mutation(),
        &Mutation::Rename("new tag".into())
    );
    assert_eq!(
        observed.selection().paths(),
        &[
            CurationFixture::path("one.wav"),
            CurationFixture::path("two.wav")
        ]
    );
    assert_eq!(observed.budget().revision(), &revision);
    assert!(observed.budget().required_metadata_bytes() > 128 * 1024 * 1024);
    let descriptor = observed.capacity_descriptor().unwrap();
    assert_eq!(
        descriptor.metadata().unwrap().len(),
        observed.budget().resident_database_bytes()
    );
    writer
        .validate_tag_admission_observation(&observed)
        .unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
}

pub(super) fn scope() {
    let (fixture, old) = prepared();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("tag-observation"),
        7,
    )
    .unwrap();
    let revision = writer.current_revision().unwrap();
    let db = &writer.database_for_tests().connection;
    db.execute_batch("PRAGMA wal_checkpoint(RESTART)").unwrap();
    // Deliberately corrupt owner-maintained truth without advancing the source revision.
    // This demonstrates full revalidation rather than trusting revision equality alone.
    for (change, restore) in [
        (
            "UPDATE source_directory_generations SET complete=0 WHERE status='active'",
            "UPDATE source_directory_generations SET complete=1 WHERE status='active'",
        ),
        (
            "UPDATE source_directory_generations SET expected_entry_count=1 WHERE status='active'",
            "UPDATE source_directory_generations SET expected_entry_count=0 WHERE status='active'",
        ),
        (
            "UPDATE source_directory_generations SET published_source_revision=0 WHERE status='active'",
            "UPDATE source_directory_generations SET published_source_revision=1 WHERE status='active'",
        ),
        (
            "CREATE TABLE unknown_tag_observation(value TEXT)",
            "DROP TABLE unknown_tag_observation",
        ),
    ] {
        let observed = writer
            .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
            .unwrap();
        db.execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.execute_batch(change).unwrap();
        db.execute_batch("PRAGMA ignore_check_constraints=OFF")
            .unwrap();
        let changes = db.total_changes();
        assert!(
            writer
                .validate_tag_admission_observation(&observed)
                .is_err(),
            "{change}"
        );
        assert_eq!(db.total_changes(), changes);
        db.execute_batch(restore).unwrap();
        db.execute("UPDATE source_directory_generations SET published_source_revision=?1 WHERE status='active'", [revision.revision()]).unwrap();
    }
    let observed = writer
        .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
        .unwrap();
    // Revalidate restored complete truth before testing membership changes.
    writer
        .validate_tag_admission_observation(&observed)
        .unwrap();
    db.execute(
        "DELETE FROM wav_file_tags WHERE tag_id=?1 AND path='two.wav'",
        [old],
    )
    .unwrap();
    assert!(
        writer
            .validate_tag_admission_observation(&observed)
            .is_err()
    );
    db.execute(
        "INSERT INTO wav_file_tags(path,tag_id) VALUES ('two.wav',?1)",
        [old],
    )
    .unwrap();
    let observed = writer
        .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
        .unwrap();
    // A complete replacement directory generation also invalidates the frozen scope.
    db.execute_batch("UPDATE source_directory_generations SET generation=2 WHERE status='active'")
        .unwrap();
    assert!(
        writer
            .validate_tag_admission_observation(&observed)
            .is_err()
    );
    db.execute_batch("UPDATE source_directory_generations SET generation=1 WHERE status='active'")
        .unwrap();
    let observed = writer
        .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
        .unwrap();
    writer
        .validate_tag_admission_observation(&observed)
        .unwrap();
    let limit = observed.budget().page_count() + 4;
    db.pragma_update(None, "max_page_count", limit).unwrap();
    assert!(
        writer
            .validate_tag_admission_observation(&observed)
            .is_err()
    );
}

pub(super) fn owner() {
    let (fixture, old) = prepared();
    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("tag-observation"),
        7,
    )
    .unwrap();
    let revision = writer.current_revision().unwrap();
    let observed = writer
        .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
        .unwrap();
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(91)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(92)),
        SourceCommandId::new(),
        revision,
        vec![SourceCurationUpdate::AssignExistingTag {
            path: CurationFixture::path("one.wav"),
            tag_id: old,
        }],
    )
    .unwrap();
    writer.commit_curation(request).unwrap();
    assert!(
        writer
            .validate_tag_admission_observation(&observed)
            .is_err()
    );
    let current = writer.current_revision().unwrap();
    let observed = writer
        .capture_tag_admission_observation(&current, "old tag", Mutation::Delete)
        .unwrap();
    let database = writer.into_catalog_database();
    assert!(matches!(
        observed.capacity_descriptor(),
        Err(SourceWriterError::StaleFence)
    ));
    let replacement =
        SourceWriter::new(database, SourceId::from_string("tag-observation"), 7).unwrap();
    assert!(matches!(
        replacement.validate_tag_admission_observation(&observed),
        Err(SourceWriterError::StaleFence)
    ));
}

#[test]
fn tag_admission_observation_excludes_extension_initialization_during_selection() {
    tag_metadata_budget::run_case("tag-observation-extension");
}

struct ExtensionProbe {
    connection: rusqlite::Connection,
    path: PathBuf,
    attempted: bool,
    refused: bool,
    tainted: bool,
}
unsafe extern "C" fn selection_extension_probe(
    data: *mut std::ffi::c_void,
    action: std::ffi::c_int,
    table: *const std::ffi::c_char,
    _column: *const std::ffi::c_char,
    _database: *const std::ffi::c_char,
    _trigger: *const std::ffi::c_char,
) -> std::ffi::c_int {
    // SAFETY: the fixture retains this Box until the callback is unregistered. SQLite invokes
    // it synchronously on the exclusively held connection, with a valid C table name.
    let probe = unsafe { &mut *data.cast::<ExtensionProbe>() };
    if action == rusqlite::ffi::SQLITE_READ
        && !table.is_null()
        && !probe.attempted
        && unsafe { std::ffi::CStr::from_ptr(table) }.to_bytes() == b"wav_file_tags"
    {
        probe.attempted = true;
        probe.refused =
            crate::sqlite_ext::load_extension_for_test(&probe.connection, &probe.path).is_err();
        probe.tainted = crate::sqlite_ext::extension_load_attempted();
    }
    rusqlite::ffi::SQLITE_OK
}

pub(super) fn extension() {
    let (fixture, _) = prepared();
    let path = fixture._source.path().join("invalid-observation-extension");
    std::fs::write(&path, b"not an extension").unwrap();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("tag-observation"),
        7,
    )
    .unwrap();
    let revision = writer.current_revision().unwrap();
    let mut probe = Box::new(ExtensionProbe {
        connection: rusqlite::Connection::open_in_memory().unwrap(),
        path,
        attempted: false,
        refused: false,
        tainted: false,
    });
    let connection = &writer.database_for_tests().connection;
    // SAFETY: callback/userdata outlive all synchronous fixture SQL and are removed before Drop.
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                connection.handle(),
                Some(selection_extension_probe),
                (&mut *probe as *mut ExtensionProbe).cast(),
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    let observed = writer.capture_tag_admission_observation(&revision, "old tag", Mutation::Delete);
    // SAFETY: no concurrent SQL; remove the callback before inspecting or dropping its userdata.
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(connection.handle(), None, std::ptr::null_mut())
        },
        rusqlite::ffi::SQLITE_OK
    );
    let observed = observed.unwrap();
    assert!(probe.attempted && probe.refused && !probe.tainted);
    assert!(!crate::sqlite_ext::extension_load_attempted());
    writer
        .validate_tag_admission_observation(&observed)
        .unwrap();
    // After the complete observation scope ends, an actual attempt taints future observations.
    assert!(crate::sqlite_ext::load_extension_for_test(&probe.connection, &probe.path).is_err());
    assert!(crate::sqlite_ext::extension_load_attempted());
    assert!(
        writer
            .validate_tag_admission_observation(&observed)
            .is_err()
    );
}
