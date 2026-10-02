use super::super::super::mutation_context::{GlobalMutationRequest, GlobalMutationValue};

fn completion(
    writer: &GlobalLibraryWriter,
    request: GlobalMutationRequest,
) -> Result<GlobalMutationValue, LibraryError> {
    let context = request.context().clone();
    writer
        .submit_captured_mutation(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&context)
}

#[test]
fn captured_mutations_reject_all_old_acquisition_routes_through_fresh_owner() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "captured-stale");
    wait_until_available(&writer);
    let old = writer.acquisition_id();
    let old_client = writer.profile_tag_vocabulary_client();
    assert_eq!(old, old_client.acquisition_id());
    let seed = GlobalMutationRequest::vocabulary_set(
        old.clone(),
        1,
        0,
        "kick".into(),
        ProfileTagCategory::SoundType,
    )
    .unwrap();
    assert!(matches!(
        completion(&writer, seed),
        Ok(GlobalMutationValue::VocabularySet(_))
    ));
    let pending = [
        GlobalMutationRequest::registry(
            old.clone(),
            2,
            SourceRegistrySnapshot::new(vec![source(&temp.path().join("obsolete"), "obsolete")]),
        )
        .unwrap(),
        GlobalMutationRequest::preferences(old.clone(), 3, "obsolete".into()).unwrap(),
        GlobalMutationRequest::vocabulary_set(
            old.clone(),
            4,
            1,
            "kick".into(),
            ProfileTagCategory::Character,
        )
        .unwrap(),
        GlobalMutationRequest::vocabulary_delete(old.clone(), 5, 1, "kick".into()).unwrap(),
    ];
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let fresh = WritableProfileGuard::acquire_current().unwrap();
    let mut reopened = GlobalLibraryWriter::start(&fresh).unwrap();
    wait_until_available(&reopened);
    assert_ne!(old, reopened.acquisition_id());
    for request in pending {
        assert!(matches!(
            completion(&reopened, request),
            Err(LibraryError::GlobalMutationStaleAcquisition)
        ));
    }
    let delayed =
        GlobalMutationRequest::vocabulary_delete(old.clone(), 6, 1, "kick".into()).unwrap();
    let expected = delayed.context().clone();
    let current_client = reopened.profile_tag_vocabulary_client();
    let result = current_client
        .submit_captured_mutation(delayed.clone())
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&expected);
    assert!(matches!(
        result,
        Err(LibraryError::GlobalMutationStaleAcquisition)
    ));
    assert!(matches!(
        old_client.submit_captured_mutation(delayed),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert!(
        reopened
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .is_none()
    );
    let state = reopened
        .lookup_profile_tag_categories(vec!["kick".into()])
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.entries[0].category, ProfileTagCategory::SoundType);
    assert!(reopened.shutdown());
}

#[test]
fn captured_mutations_echo_context_and_commit_all_current_routes_with_existing_cas() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, _guard, mut writer) = start_writer(&temp, "captured-current");
    wait_until_available(&writer);
    let acquisition = writer.acquisition_id();
    let registry = GlobalMutationRequest::registry(
        acquisition.clone(),
        1,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("current"), "current")]),
    )
    .unwrap();
    assert!(matches!(
        completion(&writer, registry),
        Ok(GlobalMutationValue::RegistryCommitted)
    ));
    let prefs =
        GlobalMutationRequest::preferences(acquisition.clone(), 2, "current".into()).unwrap();
    assert!(matches!(
        completion(&writer, prefs),
        Ok(GlobalMutationValue::PreferencesCommitted)
    ));
    let set = GlobalMutationRequest::vocabulary_set(
        acquisition.clone(),
        3,
        0,
        "kick".into(),
        ProfileTagCategory::SoundType,
    )
    .unwrap();
    let stale = set.clone();
    assert!(matches!(
        completion(&writer, set),
        Ok(GlobalMutationValue::VocabularySet(
            ProfileTagVocabularySetReceipt {
                revision: 1,
                changed: true
            }
        ))
    ));
    // Captured context adds a live fence, not a durable vocabulary replay ledger.
    assert!(matches!(
        completion(&writer, stale),
        Err(LibraryError::ProfileTagVocabularyStaleRevision {
            expected: 0,
            actual: 1
        })
    ));
    let deletion =
        GlobalMutationRequest::vocabulary_delete(acquisition, 4, 1, "kick".into()).unwrap();
    let expected = deletion.context().clone();
    let value = writer
        .profile_tag_vocabulary_client()
        .submit_captured_mutation(deletion)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&expected)
        .unwrap();
    let GlobalMutationValue::VocabularyDelete(receipt) = value else {
        panic!("wrong route");
    };
    assert_eq!(receipt.revision, 2);
    assert_eq!(receipt.removed.unwrap().label, "kick");
    assert_eq!(
        writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .as_slice()[0]
            .id
            .as_str(),
        "current"
    );
    assert_eq!(
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .as_deref(),
        Some("current")
    );
    assert!(writer.shutdown());
}

#[test]
fn captured_completion_must_match_acquisition_route_correlation_and_payload() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, _guard, mut writer) = start_writer(&temp, "captured-correlation");
    wait_until_available(&writer);
    let id = writer.acquisition_id();
    let proposal = GlobalMutationRequest::preferences(id.clone(), 7, "correct".into()).unwrap();
    let wrong = [
        GlobalMutationRequest::preferences(id.clone(), 8, "correct".into()).unwrap(),
        GlobalMutationRequest::preferences(id.clone(), 7, "wrong".into()).unwrap(),
        GlobalMutationRequest::registry(id.clone(), 7, SourceRegistrySnapshot::new(vec![]))
            .unwrap(),
        GlobalMutationRequest::preferences(ProfileAcquisitionId::test_only(), 7, "correct".into())
            .unwrap(),
    ];
    for other in wrong {
        let reply = writer
            .submit_captured_mutation(proposal.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            reply.into_result_for(other.context()),
            Err(LibraryError::GlobalMutationCompletionMismatch)
        ));
    }
    assert!(matches!(
        completion(&writer, proposal),
        Ok(GlobalMutationValue::PreferencesCommitted)
    ));
    assert!(writer.shutdown());
}

#[test]
fn captured_requests_keep_bounds_queue_backpressure_and_client_route_limits() {
    let writer = GlobalLibraryWriter::disabled();
    let id = writer.acquisition_id();
    assert!(matches!(
        GlobalMutationRequest::preferences(id.clone(), 1, "x".repeat(UI_PREFERENCES_MAX_BYTES + 1)),
        Err(GlobalLibraryWriterQueueError::PayloadTooLarge { .. })
    ));
    assert!(matches!(
        GlobalMutationRequest::vocabulary_set(
            id.clone(),
            1,
            0,
            "x".repeat(1025),
            ProfileTagCategory::SoundType
        ),
        Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
            _
        ))
    ));
    assert!(matches!(
        GlobalMutationRequest::vocabulary_delete(id.clone(), 1, 0, "loop".into()),
        Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
            _
        ))
    ));
    let snapshot = SourceRegistrySnapshot::new(
        (0..=SOURCE_REGISTRY_MAX_DESCRIPTORS)
            .map(|i| source(Path::new("a"), &format!("id-{i}")))
            .collect(),
    );
    assert!(matches!(
        GlobalMutationRequest::registry(id.clone(), 1, snapshot),
        Err(GlobalLibraryWriterQueueError::SourceRegistryTooManyDescriptors { .. })
    ));
    let request = GlobalMutationRequest::preferences(id, 1, "bounded".into()).unwrap();
    assert!(matches!(
        writer
            .profile_tag_vocabulary_client()
            .submit_captured_mutation(request.clone()),
        Err(GlobalLibraryWriterQueueError::InvalidMutationRoute)
    ));
    let mut receivers = Vec::new();
    for _ in 0..GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
        receivers.push(writer.submit_captured_mutation(request.clone()).unwrap());
    }
    assert!(matches!(
        writer.submit_captured_mutation(request),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
}

#[cfg(unix)]
#[test]
fn captured_mutation_post_commit_profile_change_and_queued_rejection_echo_original_context() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("captured-post-check");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let (mut writer, ready, release) =
        GlobalLibraryWriter::start_with_post_command_gate_for_test(&guard);
    wait_until_available(&writer);
    let first = GlobalMutationRequest::preferences(
        writer.acquisition_id(),
        1,
        "committed-but-unconfirmed".into(),
    )
    .unwrap();
    let first_context = first.context().clone();
    let result = writer.submit_captured_mutation(first).unwrap();
    ready.wait();
    let queued =
        GlobalMutationRequest::preferences(writer.acquisition_id(), 2, "never-committed".into())
            .unwrap();
    let queued_context = queued.context().clone();
    let queued_result = writer.submit_captured_mutation(queued).unwrap();
    let root = guard.profile_root().to_path_buf();
    let displaced = temp.path().join("displaced");
    fs::rename(&root, &displaced).unwrap();
    fs::create_dir(&root).unwrap();
    release.wait();
    let outcome = result
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&first_context);
    assert!(
        matches!(outcome, Err(LibraryError::ProfileOwnershipChanged { reason, .. }) if reason.contains("completion not confirmable"))
    );
    let outcome = queued_result
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&queued_context);
    assert!(matches!(
        outcome,
        Err(LibraryError::ProfileOwnershipChanged { .. })
    ));
    wait_until_closed(&writer);
    let db = rusqlite::Connection::open(displaced.join(super::super::super::LIBRARY_DB_FILE_NAME))
        .unwrap();
    let value: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='ui_preferences_v1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "committed-but-unconfirmed");
    assert!(!writer.shutdown());
}
