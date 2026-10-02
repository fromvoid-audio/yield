use super::*;

fn assert_unknown<T: std::fmt::Debug>(result: Result<T, LibraryError>) {
    assert!(
        matches!(
            result,
            Err(LibraryError::WalAdmission(
                WalAdmissionError::ObservationUnavailable { .. }
            ))
        ),
        "{result:?}"
    );
}

#[test]
fn global_wal_unknown_mode_rejects_all_mutations_but_preserves_cas_and_no_ops() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("global-wal-unknown");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    database
        .replace_ui_preferences_payload("last good")
        .unwrap();
    database
        .set_profile_tag_category(0, "kick", ProfileTagCategory::SoundType)
        .unwrap();
    database
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    assert_unknown(database.replace_ui_preferences_payload("new"));
    assert_unknown(database.replace_source_registry(&SourceRegistrySnapshot::new(vec![])));
    assert_unknown(database.set_profile_tag_category(1, "kick", ProfileTagCategory::Character));
    assert_unknown(database.set_profile_tag_category(1, "snare", ProfileTagCategory::SoundType));
    assert_unknown(database.delete_profile_tag_vocabulary(1, "kick"));
    assert_eq!(
        database.load_ui_preferences_payload().unwrap().as_deref(),
        Some("last good")
    );
    assert!(
        !database
            .set_profile_tag_category(1, "kick", ProfileTagCategory::SoundType)
            .unwrap()
            .changed
    );
    assert!(
        database
            .delete_profile_tag_vocabulary(1, "absent")
            .unwrap()
            .removed
            .is_none()
    );
    assert!(matches!(
        database.set_profile_tag_category(0, "kick", ProfileTagCategory::Character),
        Err(LibraryError::ProfileTagVocabularyStaleRevision { actual: 1, .. })
    ));
    database
        .connection
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    database.replace_ui_preferences_payload("new").unwrap();
    assert_eq!(
        database
            .set_profile_tag_category(1, "kick", ProfileTagCategory::Character)
            .unwrap()
            .revision,
        2
    );
}

#[cfg(unix)]
#[test]
fn global_wal_gate_uses_held_descriptor_after_replacement_and_retry_after_actual_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("global-wal-held");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    database.replace_ui_preferences_payload("initial").unwrap();
    let path = guard.profile_root().join(LIBRARY_DB_FILE_NAME);
    let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM metadata;")
        .unwrap();
    let mut last = String::new();
    let mut reached = false;
    for revision in 1..=1024 {
        let payload = format!(
            "{revision:08}{}",
            if revision % 2 == 0 { "x" } else { "y" }.repeat(UI_PREFERENCES_MAX_BYTES - 8)
        );
        match database.replace_ui_preferences_payload(&payload) {
            Ok(()) => last = payload,
            Err(LibraryError::WalAdmission(WalAdmissionError::HardWatermark { .. })) => {
                reached = true;
                break;
            }
            result => panic!("{result:?}"),
        }
    }
    assert!(reached);
    let before = crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap();
    let wal = guard
        .profile_root()
        .join(format!("{LIBRARY_DB_FILE_NAME}-wal"));
    let held = guard.profile_root().join("held-wal");
    std::fs::rename(&wal, &held).unwrap();
    std::fs::write(&wal, []).unwrap();
    let result = database.replace_ui_preferences_payload("retry");
    // Always restore before any assertion/teardown: SQLite retains the genuine old descriptor.
    std::fs::remove_file(&wal).unwrap();
    std::fs::rename(&held, &wal).unwrap();
    assert!(
        matches!(result, Err(LibraryError::WalAdmission(WalAdmissionError::HardWatermark {
        observed_bytes, ..
    })) if observed_bytes == before)
    );
    assert_eq!(
        database.load_ui_preferences_payload().unwrap().unwrap(),
        last
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    database
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert_eq!(
        crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap(),
        0
    );
    database.replace_ui_preferences_payload("retry").unwrap();
    assert_eq!(
        database.load_ui_preferences_payload().unwrap().as_deref(),
        Some("retry")
    );
}
