// OPT-2031: actual current-API evidence for the ordinary metadata/CAS decision.
// These fixtures do not implement the proposed common envelope or grant physical authority.

fn receive<T>(receiver: Receiver<Result<T, LibraryError>>) -> Result<T, LibraryError> {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("owner response")
}

#[test]
fn ordinary_snapshot_response_loss_reloads_latest_committed_value_after_reopen() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "ordinary-snapshot-policy");
    wait_until_available(&writer);
    drop(
        writer
            .replace_ui_preferences_payload("earlier".to_owned())
            .expect("accepted lost reply"),
    );
    receive(
        writer
            .replace_ui_preferences_payload("latest".to_owned())
            .expect("latest queue"),
    )
    .expect("latest commit");
    assert!(writer.shutdown());
    let mut reopened = GlobalLibraryWriter::start(&guard).expect("reopen");
    wait_until_available(&reopened);
    assert_eq!(
        receive(reopened.load_ui_preferences_payload().expect("reload"))
            .expect("loaded")
            .as_deref(),
        Some("latest")
    );
    // Reopen reads committed state. It does not manufacture an old request receipt or resubmit
    // the lost earlier proposal over that state. Context/payload fencing is follow-up work.
    assert!(reopened.shutdown());
}

#[test]
fn vocabulary_set_response_loss_is_stale_cas_not_durable_receipt_replay() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "ordinary-vocabulary-set");
    wait_until_available(&writer);
    drop(
        writer
            .set_profile_tag_category(0, "kick".to_owned(), ProfileTagCategory::SoundType)
            .expect("accepted lost reply"),
    );
    let lookup = receive(
        writer
            .lookup_profile_tag_categories(vec!["kick".to_owned()])
            .expect("barrier lookup"),
    )
    .expect("lookup");
    assert_eq!(lookup.revision, 1);
    assert_eq!(lookup.entries[0].category, ProfileTagCategory::SoundType);
    for category in [ProfileTagCategory::SoundType, ProfileTagCategory::Character] {
        assert!(matches!(
            receive(
                writer
                    .set_profile_tag_category(0, "kick".to_owned(), category)
                    .expect("retry queue")
            ),
            Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: 0,
                actual: 1
            })
        ));
    }
    assert!(writer.shutdown());
    let mut reopened = GlobalLibraryWriter::start(&guard).expect("reopen");
    wait_until_available(&reopened);
    assert!(matches!(
        receive(
            reopened
                .set_profile_tag_category(0, "kick".to_owned(), ProfileTagCategory::SoundType)
                .expect("restart retry")
        ),
        Err(LibraryError::ProfileTagVocabularyStaleRevision {
            expected: 0,
            actual: 1
        })
    ));
    let confirmed_current = receive(
        reopened
            .set_profile_tag_category(1, "kick".to_owned(), ProfileTagCategory::SoundType)
            .expect("current-state noop"),
    )
    .expect("noop");
    assert_eq!(
        confirmed_current,
        ProfileTagVocabularySetReceipt {
            revision: 1,
            changed: false
        }
    );
    // This fresh CAS result confirms current state; it is not the lost changed=true result.
    assert!(reopened.shutdown());
}

#[test]
fn vocabulary_delete_response_loss_cannot_reconstruct_removed_row_or_undo() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "ordinary-vocabulary-delete");
    wait_until_available(&writer);
    receive(
        writer
            .set_profile_tag_category(0, "kick".to_owned(), ProfileTagCategory::SoundType)
            .expect("seed queue"),
    )
    .expect("seed");
    drop(
        writer
            .delete_profile_tag_vocabulary(1, "kick".to_owned())
            .expect("accepted lost delete reply"),
    );
    let lookup = receive(
        writer
            .lookup_profile_tag_categories(vec!["kick".to_owned()])
            .expect("barrier lookup"),
    )
    .expect("lookup");
    assert_eq!(lookup.revision, 2);
    assert!(lookup.entries.is_empty());
    assert!(writer.shutdown());
    let mut reopened = GlobalLibraryWriter::start(&guard).expect("reopen");
    wait_until_available(&reopened);
    assert!(matches!(
        receive(
            reopened
                .delete_profile_tag_vocabulary(1, "kick".to_owned())
                .expect("old CAS")
        ),
        Err(LibraryError::ProfileTagVocabularyStaleRevision {
            expected: 1,
            actual: 2
        })
    ));
    let current_noop = receive(
        reopened
            .delete_profile_tag_vocabulary(2, "kick".to_owned())
            .expect("new current CAS"),
    )
    .expect("noop");
    assert_eq!(
        current_noop,
        ProfileTagVocabularyDeleteReceipt {
            revision: 2,
            removed: None
        }
    );
    // Absence is not the historical removed row. A UI may not invent an Undo tombstone from it.
    assert!(reopened.shutdown());
}

#[test]
fn old_vocabulary_client_cannot_mutate_a_reopened_profile_owner() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "ordinary-acquisition-fence");
    wait_until_available(&writer);
    let old_client = writer.profile_tag_vocabulary_client();
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let new_guard = WritableProfileGuard::acquire_current().expect("fresh acquisition");
    let mut reopened = GlobalLibraryWriter::start(&new_guard).expect("fresh owner");
    wait_until_available(&reopened);
    assert!(matches!(
        old_client.set_profile_tag_category(0, "kick".to_owned(), ProfileTagCategory::SoundType),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert!(matches!(
        old_client.delete_profile_tag_vocabulary(0, "kick".to_owned()),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    let lookup = receive(
        reopened
            .lookup_profile_tag_categories(vec!["kick".to_owned()])
            .expect("new lookup"),
    )
    .expect("new state");
    assert_eq!(lookup.revision, 0);
    assert!(lookup.entries.is_empty());
    assert!(reopened.shutdown());
}
