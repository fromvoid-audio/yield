use super::super::super::mutation_context::{GlobalMutationRequest, GlobalMutationValue};

fn request(
    writer: &GlobalLibraryWriter,
    preferences: bool,
    revision: u64,
    value: &str,
) -> GlobalMutationRequest {
    if preferences {
        GlobalMutationRequest::preferences(writer.acquisition_id(), revision, value.into()).unwrap()
    } else {
        GlobalMutationRequest::registry(
            writer.acquisition_id(),
            revision,
            SourceRegistrySnapshot::new(vec![source(Path::new(value), value)]),
        )
        .unwrap()
    }
}

fn apply(
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

fn loaded(writer: &GlobalLibraryWriter, preferences: bool) -> String {
    if preferences {
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .unwrap()
    } else {
        writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .as_slice()[0]
            .id
            .as_str()
            .into()
    }
}

#[test]
fn snapshot_slots_reject_lower_and_conflicting_revisions_replay_without_reapplying() {
    let _runtime = TestRuntimeGuard::acquire();
    for preferences in [false, true] {
        let temp = tempdir().unwrap();
        let (_base, _profile, guard, mut writer) = start_writer(&temp, "snapshot-order");
        wait_until_available(&writer);
        let original = request(&writer, preferences, 2, "latest");
        assert!(apply(&writer, original.clone()).is_ok());
        // A duplicate must not execute SQL again, even after an unrelated unframed update.
        if preferences {
            writer
                .replace_ui_preferences_payload("independent".into())
                .unwrap()
                .recv()
                .unwrap()
                .unwrap();
        } else {
            writer
                .replace_source_registry(SourceRegistrySnapshot::new(vec![source(
                    Path::new("independent"),
                    "independent",
                )]))
                .unwrap()
                .recv()
                .unwrap()
                .unwrap();
        }
        let connection = rusqlite::Connection::open(
            guard
                .profile_root()
                .join(super::super::super::LIBRARY_DB_FILE_NAME),
        )
        .unwrap();
        connection.execute_batch(if preferences {
            "CREATE TRIGGER reject_repeat BEFORE INSERT ON metadata BEGIN SELECT RAISE(FAIL, 'unexpected reapply'); END;"
        } else {
            "CREATE TRIGGER reject_repeat BEFORE DELETE ON sources BEGIN SELECT RAISE(FAIL, 'unexpected reapply'); END;"
        }).unwrap();
        assert!(apply(&writer, original).is_ok());
        assert!(matches!(
            apply(&writer, request(&writer, preferences, 1, "obsolete")),
            Err(LibraryError::GlobalSnapshotStaleRevision {
                proposed: 1,
                committed: 2
            })
        ));
        assert!(matches!(
            apply(&writer, request(&writer, preferences, 2, "changed")),
            Err(LibraryError::GlobalSnapshotPayloadConflict { revision: 2 })
        ));
        assert_eq!(loaded(&writer, preferences), "independent");
        connection
            .execute_batch("DROP TRIGGER reject_repeat")
            .unwrap();
        assert!(apply(&writer, request(&writer, preferences, 3, "next")).is_ok());
        assert_eq!(loaded(&writer, preferences), "next");
        assert!(writer.shutdown());
    }
}

#[test]
fn snapshot_lost_response_replays_both_series_and_reopen_resets_only_local_fences() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "snapshot-restart");
    wait_until_available(&writer);
    let registry = request(&writer, false, 100, "sources");
    let preferences = request(&writer, true, 1, "preferences");
    drop(writer.submit_captured_mutation(registry.clone()).unwrap());
    drop(
        writer
            .submit_captured_mutation(preferences.clone())
            .unwrap(),
    );
    // FIFO load observes accepted work even after its result receiver has been lost.
    assert_eq!(loaded(&writer, false), "sources");
    assert_eq!(loaded(&writer, true), "preferences");
    for proposal in [registry.clone(), preferences.clone()] {
        assert!(apply(&writer, proposal).is_ok());
    }
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut reopened = GlobalLibraryWriter::start(&guard).unwrap();
    wait_until_available(&reopened);
    assert_eq!(loaded(&reopened, false), "sources");
    assert_eq!(loaded(&reopened, true), "preferences");
    for proposal in [registry, preferences] {
        assert!(matches!(
            apply(&reopened, proposal),
            Err(LibraryError::GlobalMutationStaleAcquisition)
        ));
    }
    // New producer loads current state and starts fresh; old local ordering isn't durable.
    assert!(apply(&reopened, request(&reopened, false, 0, "fresh-sources")).is_ok());
    assert!(apply(&reopened, request(&reopened, true, 0, "fresh-preferences")).is_ok());
    assert_eq!(loaded(&reopened, false), "fresh-sources");
    assert_eq!(loaded(&reopened, true), "fresh-preferences");
    assert!(reopened.shutdown());
}

#[test]
fn snapshot_failed_transaction_does_not_advance_or_poison_acknowledgement_slot() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "snapshot-rollback");
    wait_until_available(&writer);
    assert!(apply(&writer, request(&writer, true, 1, "baseline")).is_ok());
    let connection = rusqlite::Connection::open(
        guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME),
    )
    .unwrap();
    connection.execute_batch("CREATE TRIGGER reject_pref BEFORE INSERT ON metadata BEGIN SELECT RAISE(FAIL, 'pref fault'); END;").unwrap();
    assert!(apply(&writer, request(&writer, true, 2, "failed")).is_err());
    assert_eq!(loaded(&writer, true), "baseline");
    assert!(apply(&writer, request(&writer, true, 1, "baseline")).is_ok());
    connection
        .execute_batch("DROP TRIGGER reject_pref")
        .unwrap();
    // Same revision with a different payload succeeds because the failed proposal never committed.
    assert!(apply(&writer, request(&writer, true, 2, "reconciled")).is_ok());
    assert_eq!(loaded(&writer, true), "reconciled");
    assert!(writer.shutdown());
}
