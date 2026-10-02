use super::*;
use crate::sample_sources::library::mutation_context::SnapshotFences;

#[cfg(unix)]
#[test]
fn ordinary_global_wal_actual_worker_replacement_during_checkpoint_is_not_confirmed() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-during-replacement");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = guard.profile_root().to_path_buf();
    let displaced = temp.path().join("displaced-during-checkpoint");
    let counter = Arc::new(AtomicUsize::new(0));
    let setup = pressure_hook(counter.clone(), Fault::None);
    let hook_counter = counter.clone();
    let hook_root = root.clone();
    let hook = Arc::new(move |connection: &Connection| {
        setup(connection);
        let counter = hook_counter.clone();
        let root = hook_root.clone();
        let displaced = displaced.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                }
            ) {
                counter.fetch_add(1, Ordering::SeqCst);
                std::fs::rename(&root, &displaced).unwrap();
                std::fs::create_dir(&root).unwrap();
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    });
    let mut writer =
        GlobalLibraryWriter::start_with_database_hook_for_test(&guard, hook, None).unwrap();
    available(&writer);
    let result = complete(
        &writer,
        GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "committed".into()).unwrap(),
    );
    assert!(
        matches!(result, Err(LibraryError::ProfileOwnershipChanged { .. })),
        "replacement during maintenance cannot be acknowledged as current: {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    closed_after_profile_change(&writer);
    assert!(!writer.shutdown());
    assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
}

#[test]
fn ordinary_global_wal_markers_exclude_exact_replay_conflict_stale_and_active_transactions() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-markers");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let mut fences = SnapshotFences::default();
    let acquisition = ProfileAcquisitionId::from_guard(&guard);
    let request =
        GlobalMutationRequest::preferences(acquisition.clone(), 2, "original".into()).unwrap();
    let context = request.context().clone();
    request
        .clone()
        .execute(&mut database, guard.owner_identity(), &mut fences)
        .into_result_for(&context)
        .unwrap();
    assert!(database.ordinary_wal_maintenance.committed.get());
    database.maintain_after_ordinary_commit(&guard);
    assert!(!database.ordinary_wal_maintenance.committed.get());
    request
        .execute(&mut database, guard.owner_identity(), &mut fences)
        .into_result_for(&context)
        .unwrap();
    assert!(
        !database.ordinary_wal_maintenance.committed.get(),
        "replay must not pretend a new commit occurred"
    );
    for request in [
        GlobalMutationRequest::preferences(acquisition.clone(), 1, "stale".into()).unwrap(),
        GlobalMutationRequest::preferences(acquisition, 2, "conflicting".into()).unwrap(),
    ] {
        let context = request.context().clone();
        assert!(
            request
                .execute(&mut database, guard.owner_identity(), &mut fences)
                .into_result_for(&context)
                .is_err()
        );
        assert!(!database.ordinary_wal_maintenance.committed.get());
    }
    assert_eq!(
        database.load_ui_preferences_payload().unwrap().as_deref(),
        Some("original")
    );
    database
        .set_profile_tag_category(0, "kick", ProfileTagCategory::SoundType)
        .unwrap();
    assert!(database.ordinary_wal_maintenance.committed.get());
    database.maintain_after_ordinary_commit(&guard);
    assert!(
        !database
            .set_profile_tag_category(1, "kick", ProfileTagCategory::SoundType)
            .unwrap()
            .changed
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert!(
        database
            .delete_profile_tag_vocabulary(1, "missing")
            .unwrap()
            .removed
            .is_none()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert!(
        database
            .delete_profile_tag_vocabulary(1, "kick")
            .unwrap()
            .removed
            .is_some()
    );
    assert!(database.ordinary_wal_maintenance.committed.get());
    // A later exclusive reservation must prevent maintenance from issuing checkpoint SQL.
    database
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    database.maintain_after_ordinary_commit(&guard);
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert!(!database.ordinary_wal_maintenance.policy.has_attempted());
    database.connection.execute_batch("ROLLBACK").unwrap();
    drop(database);
    let reopened = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    assert!(!reopened.ordinary_wal_maintenance.committed.get());
    assert!(!reopened.ordinary_wal_maintenance.policy.has_attempted());
    assert_eq!(
        reopened.load_ui_preferences_payload().unwrap().as_deref(),
        Some("original")
    );
}

#[cfg(unix)]
#[test]
fn ordinary_global_wal_actual_worker_replacement_before_postvalidation_skips_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-replacement");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        Some((ready.clone(), release.clone())),
    )
    .unwrap();
    available(&writer);
    let request =
        GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "committed".into()).unwrap();
    let context = request.context().clone();
    let result = writer.submit_captured_mutation(request).unwrap();
    ready.wait();
    let root = guard.profile_root().to_path_buf();
    std::fs::rename(&root, temp.path().join("displaced-profile")).unwrap();
    std::fs::create_dir(&root).unwrap();
    release.wait();
    assert!(matches!(
        result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .into_result_for(&context),
        Err(LibraryError::ProfileOwnershipChanged { .. })
    ));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "postvalidation fails before maintenance can execute"
    );
    closed_after_profile_change(&writer);
    assert!(!writer.shutdown());
    assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
}
