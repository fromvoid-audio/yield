//! Original SQLite selection comparison and source transport capacity without physical grants.
use super::tag_wal_preparation::{db, request, settings, writer};
use super::*;
use crate::operation_journal::{AdmissionClaimId, ParticipantId};
use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
use crate::sample_sources::db::write::tag_assignment_selection;
use crate::sample_sources::wal_transport::{
    SourceWalTransport, SourceWalTransportCorrelation, SourceWalTransportRejection as Rejection,
    SourceWalTransportStatus as Status,
};
macro_rules! case {
    ($name:ident,$case:literal) => {
        #[test]
        fn $name() {
            tag_metadata_budget::run_case($case);
        }
    };
}
case!(
    streaming_complete_selection_checks_every_current_row_without_a_second_payload,
    "tag-streaming-rows"
);
case!(
    streaming_selection_rejects_missing_extra_orphan_unsupported_and_noncanonical_rows,
    "tag-streaming-faults"
);
case!(
    streaming_selection_preserves_old_id_rename_collision_and_exact_mutation_limits,
    "tag-streaming-identity"
);
case!(
    streaming_large_original_selection_remains_valid_and_shared_full_refusal_preserves_owner,
    "tag-streaming-large"
);
case!(streaming_transport_native_staging_uses_original_pool_and_never_empty_closes_preexisting_transaction,"tag-streaming-staging");
case!(
    streaming_typed_transport_keeps_original_binding_copies_charged_until_last_drop,
    "tag-streaming-copies"
);
case!(
    streaming_schema_and_runtime_refusal_do_not_collect_large_current_images,
    "tag-streaming-schema"
);
pub(super) fn dispatch(case: &str) {
    match case {
        "tag-streaming-rows" => rows(),
        "tag-streaming-faults" => faults(),
        "tag-streaming-identity" => identity(),
        "tag-streaming-large" => large(),
        "tag-streaming-staging" => staging(),
        "tag-streaming-copies" => copies(),
        "tag-streaming-schema" => schema(),
        _ => panic!("unknown streaming case"),
    }
}
fn correlation() -> SourceWalTransportCorrelation {
    SourceWalTransportCorrelation::new(
        [63; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap()
}
fn observe(
    w: &SourceWriter,
    mutation: Mutation,
) -> crate::sample_sources::db::SourceTagAdmissionObservation {
    w.capture_tag_admission_observation(&w.current_revision().unwrap(), "old tag", mutation)
        .unwrap()
}
fn seed(w: &SourceWriter, old: i64, count: usize, path_bytes: usize) {
    db(w)
        .execute("DELETE FROM wav_file_tags WHERE tag_id=?1", [old])
        .unwrap();
    for i in (0..count).rev() {
        let path = format!("{i:03}/{}/file.wav", "x".repeat(path_bytes));
        db(w)
            .execute(
                "INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES (?1,1,1,'wav')",
                [&path],
            )
            .unwrap();
        db(w)
            .execute(
                "INSERT INTO wav_file_tags(path,tag_id) VALUES (?1,?2)",
                rusqlite::params![path, old],
            )
            .unwrap();
    }
}
fn rows() {
    for (count, mutation) in [
        (64, Mutation::Rename("new tag".into())),
        (128, Mutation::Delete),
    ] {
        let (w, _fixture, old) = writer();
        seed(&w, old, count, 32);
        let observation = observe(&w, mutation);
        db(&w)
            .pragma_update(None, "reverse_unordered_selects", true)
            .unwrap();
        let before = source_writer_state_snapshot(w.database_for_tests());
        let changes = db(&w).total_changes();
        tag_assignment_selection::validate_current(db(&w), observation.selection()).unwrap();
        w.validate_tag_admission_observation(&observation).unwrap();
        assert_eq!(db(&w).total_changes(), changes);
        assert_eq!(source_writer_state_snapshot(w.database_for_tests()), before);
        let prepared = w
            .prepare_tag_wal_reservation(observation, request())
            .unwrap();
        prepared.revalidate().unwrap();
        let w = prepared.abort().unwrap();
        assert!(db(&w).is_autocommit());
    }
    let (w, _fixture, old) = writer();
    db(&w)
        .execute("DELETE FROM wav_file_tags WHERE tag_id=?1", [old])
        .unwrap();
    let observation = observe(&w, Mutation::Delete);
    assert!(observation.selection().paths().is_empty());
    tag_assignment_selection::validate_current(db(&w), observation.selection()).unwrap();
}
fn faults() {
    for (change, restore) in [
        (
            "DELETE FROM wav_file_tags WHERE path='two.wav'",
            "INSERT INTO wav_file_tags(path,tag_id) SELECT 'two.wav',id FROM source_tags WHERE normalized_text='old tag'",
        ),
        (
            "INSERT INTO wav_file_tags(path,tag_id) SELECT 'three.wav',id FROM source_tags WHERE normalized_text='old tag'",
            "DELETE FROM wav_file_tags WHERE path='three.wav'",
        ),
        (
            "PRAGMA foreign_keys=OFF; DELETE FROM wav_files WHERE path='two.wav'",
            "INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES ('two.wav',1,1,'wav')",
        ),
        (
            "UPDATE wav_files SET extension='mp3' WHERE path='two.wav'",
            "UPDATE wav_files SET extension='wav' WHERE path='two.wav'",
        ),
        (
            "UPDATE wav_files SET missing=1 WHERE path='two.wav'",
            "UPDATE wav_files SET missing=0 WHERE path='two.wav'",
        ),
        (
            "PRAGMA foreign_keys=OFF; UPDATE wav_file_tags SET path='./two.wav' WHERE path='two.wav'",
            "UPDATE wav_file_tags SET path='two.wav' WHERE path='./two.wav'",
        ),
        (
            "PRAGMA foreign_keys=OFF; UPDATE wav_files SET path='./two.wav' WHERE path='two.wav'; UPDATE wav_file_tags SET path='./two.wav' WHERE path='two.wav'",
            "UPDATE wav_files SET path='two.wav' WHERE path='./two.wav'; UPDATE wav_file_tags SET path='two.wav' WHERE path='./two.wav'",
        ),
        (
            "PRAGMA foreign_keys=OFF; UPDATE wav_files SET path='two.mp3' WHERE path='two.wav'; UPDATE wav_file_tags SET path='two.mp3' WHERE path='two.wav'",
            "UPDATE wav_files SET path='two.wav' WHERE path='two.mp3'; UPDATE wav_file_tags SET path='two.wav' WHERE path='two.mp3'",
        ),
        (
            "ALTER TABLE wav_file_tags RENAME TO preserved_assignment_rows; CREATE VIEW wav_file_tags AS SELECT path,tag_id FROM preserved_assignment_rows UNION ALL SELECT path,tag_id FROM preserved_assignment_rows WHERE path='one.wav'",
            "DROP VIEW wav_file_tags; ALTER TABLE preserved_assignment_rows RENAME TO wav_file_tags",
        ),
        (
            "UPDATE source_tags SET normalized_text='renamed old' WHERE normalized_text='old tag'",
            "UPDATE source_tags SET normalized_text='old tag' WHERE normalized_text='renamed old'",
        ),
    ] {
        let (w, _fixture, _) = writer();
        // Valid unassigned manifest member makes the extra-assignment fault exercise
        // exact set comparison rather than fail at SQLite's foreign-key constraint.
        db(&w).execute("INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES ('three.wav',1,1,'wav')", []).unwrap();
        let observation = observe(&w, Mutation::Delete);
        db(&w)
            .execute_batch(change)
            .unwrap_or_else(|e| panic!("fault {change}: {e}"));
        let changes = db(&w).total_changes();
        assert!(
            tag_assignment_selection::validate_current(db(&w), observation.selection()).is_err(),
            "{change}"
        );
        assert!(
            w.validate_tag_admission_observation(&observation).is_err(),
            "{change}"
        );
        assert_eq!(db(&w).total_changes(), changes);
        db(&w).execute_batch(restore).unwrap();
        tag_assignment_selection::validate_current(db(&w), observation.selection()).unwrap();
    }
}
fn identity() {
    let (w, _fixture, old) = writer();
    let observation = observe(&w, Mutation::Rename("new tag".into()));
    db(&w)
        .execute(
            "INSERT INTO source_tags(normalized_text,display_label) VALUES ('new tag','New Tag')",
            [],
        )
        .unwrap();
    assert!(tag_assignment_selection::validate_current(db(&w), observation.selection()).is_err());
    db(&w)
        .execute(
            "DELETE FROM source_tags WHERE normalized_text='new tag'",
            [],
        )
        .unwrap();
    db(&w).execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db(&w)
        .execute("UPDATE source_tags SET id=id+1000 WHERE id=?1", [old])
        .unwrap();
    assert!(tag_assignment_selection::validate_current(db(&w), observation.selection()).is_err());
    for count in [64, 128] {
        let (w, _fixture, old) = writer();
        seed(&w, old, count, 8);
        let mutation = if count == 64 {
            Mutation::Rename("new tag".into())
        } else {
            Mutation::Delete
        };
        let observation = observe(&w, mutation);
        db(&w).execute("INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES ('extra.wav',1,1,'wav')",[]).unwrap();
        db(&w)
            .execute(
                "INSERT INTO wav_file_tags(path,tag_id) VALUES ('extra.wav',?1)",
                [old],
            )
            .unwrap();
        assert!(
            tag_assignment_selection::validate_current(db(&w), observation.selection()).is_err()
        );
    }
}
fn large() {
    let (w, _fixture, old) = writer();
    seed(&w, old, 128, 340);
    let observation = observe(&w, Mutation::Delete);
    assert_eq!(observation.selection().paths().len(), 128);
    let input = observation.retained_input_bytes().unwrap();
    assert!(input > 40 * 1024 && input < 60 * 1024);
    tag_assignment_selection::validate_current(db(&w), observation.selection()).unwrap();
    w.validate_tag_admission_observation(&observation).unwrap();
    let handle = unsafe { db(&w).handle() };
    let original = settings(&w);
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = transport
        .submit_tag_with_binding(observation, correlation(), request())
        .unwrap();
    assert!(transport.context_usage_for_tests().1 <= 64 * 1024);
    let owner = inbox.try_next().unwrap().acquire(w);
    // The complete conservative validation staging cannot fit with this real large input.
    // Refusal preserves the original 128-item selection limit and the actual aggregate budget.
    assert_eq!(owner.rejection(), Some(Rejection::Full));
    assert!(!owner.is_prepared());
    let w = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("unchanged source owner"));
    assert_eq!(request.status(), Status::Rejected(Rejection::Full));
    assert_eq!(unsafe { db(&w).handle() }, handle);
    assert_eq!(settings(&w), original);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
}
fn staging() {
    for preexisting in [false, true] {
        let (w, _fixture, _) = writer();
        let observation = observe(&w, Mutation::Delete);
        let revision = w.current_revision().unwrap();
        let (transport, mut inbox) = SourceWalTransport::channel();
        let request = transport
            .submit_tag_with_binding(observation, correlation(), request())
            .unwrap();
        let mut pending = Vec::new();
        loop {
            match transport.submit(
                revision.clone(),
                correlation(),
                super::tag_wal_preparation::request(),
            ) {
                Ok(r) => pending.push(r),
                Err(Rejection::Full) => break,
                Err(e) => panic!("{e}"),
            }
        }
        if preexisting {
            db(&w).execute_batch("BEGIN IMMEDIATE; INSERT INTO metadata(key,value) VALUES ('older-transaction','retained')").unwrap();
        }
        let owner = inbox.try_next().unwrap().acquire(w);
        assert_eq!(owner.rejection(), Some(Rejection::Full));
        assert!(!owner.is_prepared());
        assert!(!owner.source_authority_close_ready());
        if preexisting {
            let owner = owner
                .into_writer()
                .err()
                .expect("older transaction stays private");
            drop(owner);
            assert!(matches!(request.status(), Status::Unavailable(_)));
        } else {
            let w = owner
                .into_writer()
                .unwrap_or_else(|_| panic!("untouched original owner"));
            assert!(db(&w).is_autocommit());
            assert_eq!(request.status(), Status::Rejected(Rejection::Full));
        }
        drop(inbox);
        drop(request);
        assert_eq!(transport.context_usage_for_tests(), (0, 0));
    }
}
fn copies() {
    let (w, _fixture, _) = writer();
    let authority = w.catalog_database().prepare_source_authority().unwrap();
    let observation = observe(&w, Mutation::Rename("new tag".into()));
    let revision = w.current_revision().unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut request = transport
        .submit_tag_with_binding(observation, correlation(), request())
        .unwrap();
    let mut owner = inbox.try_next().unwrap().acquire(w);
    assert!(owner.is_prepared());
    owner.publish_prepared(Some(&authority)).unwrap();
    owner.revalidate(Some(&authority)).unwrap();
    let holder = request.try_prepared().unwrap();
    let Status::Prepared(history) = holder.status() else {
        panic!("original entered tag")
    };
    let copy = history.clone();
    assert_eq!(
        history.scope_descriptor_bytes().unwrap().as_ptr(),
        copy.scope_descriptor_bytes().unwrap().as_ptr()
    );
    let mut pending = Vec::new();
    loop {
        match transport.submit(
            revision.clone(),
            correlation(),
            super::tag_wal_preparation::request(),
        ) {
            Ok(r) => pending.push(r),
            Err(Rejection::Full) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(transport.context_usage_for_tests().1 <= 64 * 1024);
    drop(holder);
    let _w = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("original cleanup"));
    assert_eq!(request.status(), Status::Aborted);
    transport.close();
    assert_eq!(transport.context_usage_for_tests().0, 0);
    assert!(transport.context_usage_for_tests().1 > 0);
    drop(history);
    assert!(transport.context_usage_for_tests().1 > 0);
    drop(copy);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
}

fn schema() {
    for runtime in [false, true] {
        let (w, _fixture, _) = writer();
        let observation = observe(&w, Mutation::Delete);
        if runtime {
            unsafe extern "C" fn unknown_compare(
                _: *mut std::ffi::c_void,
                _: i32,
                _: *const std::ffi::c_void,
                _: i32,
                _: *const std::ffi::c_void,
            ) -> i32 {
                0
            }
            // SAFETY: exclusive actual live owner; a negative test-only callback registers
            // an unknown runtime entry. The callback borrows/dereferences no data.
            let status = unsafe {
                rusqlite::ffi::sqlite3_create_collation_v2(
                    db(&w).handle(),
                    c"streaming_unknown_collation".as_ptr(),
                    rusqlite::ffi::SQLITE_UTF8,
                    std::ptr::null_mut(),
                    Some(unknown_compare),
                    None,
                )
            };
            assert_eq!(status, rusqlite::ffi::SQLITE_OK);
        } else {
            // Actual unsupported schema above the entire handshake budget. The native
            // validation rejects borrowed membership before making an owned current image.
            for i in 0..24 {
                let sql = format!(
                    "CREATE TABLE streaming_unknown_{i}(value TEXT DEFAULT '{}')",
                    "x".repeat(4096)
                );
                db(&w).execute_batch(&sql).unwrap();
            }
            let bytes: u64 = db(&w)
                .query_row(
                    "SELECT sum(length(CAST(sql AS BLOB))) FROM sqlite_master",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(bytes > 64 * 1024);
        }
        let changes = db(&w).total_changes();
        let original = settings(&w);
        let (transport, mut inbox) = SourceWalTransport::channel();
        let request = transport
            .submit_tag_with_binding(observation, correlation(), request())
            .unwrap();
        let owner = inbox.try_next().unwrap().acquire(w);
        assert!(!owner.is_prepared());
        assert_eq!(owner.rejection(), Some(Rejection::Scope));
        let w = owner
            .into_writer()
            .unwrap_or_else(|_| panic!("original unchanged owner"));
        assert_eq!(request.status(), Status::Rejected(Rejection::Scope));
        assert_eq!(db(&w).total_changes(), changes);
        assert_eq!(settings(&w), original);
        assert_eq!(transport.context_usage_for_tests(), (0, 0));
    }
}
