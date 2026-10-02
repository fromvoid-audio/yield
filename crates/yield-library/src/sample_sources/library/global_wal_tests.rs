//! Actual owned SQLite/worker regressions; not native storage qualification.
use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::test_runtime::TestRuntimeGuard;
use crate::{WAL_HARD_WATERMARK_BYTES, WalAdmissionError};
use rusqlite::OpenFlags;
use std::time::{Duration, Instant};

fn available(writer: &GlobalLibraryWriter) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn complete(
    writer: &GlobalLibraryWriter,
    request: GlobalMutationRequest,
) -> Result<GlobalMutationValue, LibraryError> {
    let context = request.context().clone();
    writer
        .submit_captured_mutation(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .into_result_for(&context)
}

fn source(root: &std::path::Path, id: &str) -> SampleSource {
    SampleSource {
        id: SourceId::from_string(id),
        root: root.to_path_buf(),
        role: crate::sample_sources::SourceRole::Normal,
        metadata_storage: crate::sample_sources::SourceMetadataStorage::SourceFolder,
        primary_import_folder: crate::sample_sources::default_primary_import_folder(),
    }
}

fn assert_hard(result: Result<GlobalMutationValue, LibraryError>) {
    assert!(matches!(
        result,
        Err(LibraryError::WalAdmission(WalAdmissionError::HardWatermark {
            observed_bytes, hard_bytes
        })) if observed_bytes >= hard_bytes && hard_bytes == WAL_HARD_WATERMARK_BYTES
    ));
}

#[test]
fn global_wal_actual_worker_rejects_all_routes_preserves_replay_and_retries_after_reopen() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("global-wal-worker");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    available(&writer);
    let acquisition = writer.acquisition_id();
    let registry = GlobalMutationRequest::registry(
        acquisition.clone(),
        1,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("original"), "original")]),
    )
    .unwrap();
    complete(&writer, registry.clone()).unwrap();
    complete(
        &writer,
        GlobalMutationRequest::vocabulary_set(
            acquisition.clone(),
            1,
            0,
            "kick".into(),
            ProfileTagCategory::SoundType,
        )
        .unwrap(),
    )
    .unwrap();
    let initial =
        GlobalMutationRequest::preferences(acquisition.clone(), 1, "initial".into()).unwrap();
    complete(&writer, initial).unwrap();
    let path = guard.profile_root().join(LIBRARY_DB_FILE_NAME);
    let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM metadata;")
        .unwrap();

    // Grow through the real worker's bounded ordinary API while the actual external reader
    // retains its snapshot. No injected byte count, unchecked oversized SQL, or second writer.
    let mut last = None;
    let mut rejected = None;
    for revision in 2..=1024 {
        let payload = format!(
            "{revision:08}{}",
            if revision % 2 == 0 { "x" } else { "y" }.repeat(UI_PREFERENCES_MAX_BYTES - 8)
        );
        let request =
            GlobalMutationRequest::preferences(acquisition.clone(), revision, payload).unwrap();
        match complete(&writer, request.clone()) {
            Ok(_) => last = Some(request),
            Err(LibraryError::WalAdmission(error @ WalAdmissionError::HardWatermark { .. })) => {
                assert_hard(Err(LibraryError::WalAdmission(error)));
                rejected = Some(request);
                break;
            }
            other => panic!("unexpected growth result: {other:?}"),
        }
    }
    let last = last.expect("actual WAL grew through ordinary commits");
    let rejected = rejected.expect("finite growth reaches the actual hard watermark");
    let rejected_registry = GlobalMutationRequest::registry(
        acquisition.clone(),
        2,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("rejected"), "rejected")]),
    )
    .unwrap();
    let set = GlobalMutationRequest::vocabulary_set(
        acquisition.clone(),
        2,
        1,
        "kick".into(),
        ProfileTagCategory::Character,
    )
    .unwrap();
    let insert = GlobalMutationRequest::vocabulary_set(
        acquisition.clone(),
        3,
        1,
        "snare".into(),
        ProfileTagCategory::SoundType,
    )
    .unwrap();
    let delete =
        GlobalMutationRequest::vocabulary_delete(acquisition.clone(), 4, 1, "kick".into()).unwrap();
    for request in [&rejected, &rejected_registry, &set, &insert, &delete] {
        assert_hard(complete(&writer, request.clone()));
    }
    // These prove the failed proposals did not advance either snapshot fence or vocabulary CAS.
    complete(&writer, last.clone()).unwrap();
    complete(&writer, registry).unwrap();
    let current = writer
        .load_ui_preferences_payload()
        .unwrap()
        .recv()
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(current.len(), UI_PREFERENCES_MAX_BYTES);
    assert!(current.starts_with(&format!("{:08}", last.context().correlation())));
    let tags = writer
        .lookup_profile_tag_categories(vec!["kick".into(), "snare".into()])
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(tags.revision, 1);
    assert_eq!(
        tags.entries,
        vec![ProfileTagVocabularyEntry {
            label: "kick".into(),
            category: ProfileTagCategory::SoundType
        }]
    );
    let retained_registry = writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(retained_registry.as_slice().len(), 1);
    assert_eq!(
        retained_registry.as_slice()[0].id,
        SourceId::from_string("original")
    );
    assert_eq!(
        retained_registry.as_slice()[0].root,
        temp.path().join("original")
    );
    let no_op = complete(
        &writer,
        GlobalMutationRequest::vocabulary_set(
            acquisition.clone(),
            5,
            1,
            "kick".into(),
            ProfileTagCategory::SoundType,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(no_op, GlobalMutationValue::VocabularySet(receipt) if !receipt.changed && receipt.revision == 1)
    );
    let absent = complete(
        &writer,
        GlobalMutationRequest::vocabulary_delete(acquisition.clone(), 6, 1, "absent".into())
            .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(absent, GlobalMutationValue::VocabularyDelete(receipt) if receipt.removed.is_none() && receipt.revision == 1)
    );
    assert!(matches!(
        complete(
            &writer,
            GlobalMutationRequest::vocabulary_set(
                acquisition.clone(),
                7,
                0,
                "kick".into(),
                ProfileTagCategory::Character,
            )
            .unwrap()
        ),
        Err(LibraryError::ProfileTagVocabularyStaleRevision { actual: 1, .. })
    ));
    assert!(matches!(
        complete(
            &writer,
            GlobalMutationRequest::preferences(acquisition.clone(), 1, "old".into(),).unwrap()
        ),
        Err(LibraryError::GlobalSnapshotStaleRevision { .. })
    ));
    assert!(matches!(
        complete(
            &writer,
            GlobalMutationRequest::preferences(
                acquisition.clone(),
                last.context().correlation(),
                "conflict".into(),
            )
            .unwrap()
        ),
        Err(LibraryError::GlobalSnapshotPayloadConflict { .. })
    ));
    assert!(matches!(
        complete(
            &writer,
            GlobalMutationRequest::preferences(
                ProfileAcquisitionId::test_only(),
                2000,
                "stale owner".into(),
            )
            .unwrap()
        ),
        Err(LibraryError::GlobalMutationStaleAcquisition)
    ));
    // The reader still sees its original committed state, despite growth and rejection.
    let old: String = reader
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            [UI_PREFERENCES_KEY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old, "initial");
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    // Existing owner shutdown/reopen releases/checkpoints the retained WAL. This is not a new
    // soft-watermark maintenance scheduler or protected-capacity implementation.
    assert!(writer.shutdown());
    drop(writer);
    let mut reopened = GlobalLibraryWriter::start(&guard).unwrap();
    available(&reopened);
    assert_eq!(acquisition, reopened.acquisition_id());
    complete(&reopened, rejected).unwrap();
    complete(&reopened, rejected_registry).unwrap();
    complete(&reopened, set).unwrap();
    complete(
        &reopened,
        GlobalMutationRequest::vocabulary_delete(acquisition, 8, 2, "kick".into()).unwrap(),
    )
    .unwrap();
    assert!(reopened.shutdown());
}

mod database_tests;
