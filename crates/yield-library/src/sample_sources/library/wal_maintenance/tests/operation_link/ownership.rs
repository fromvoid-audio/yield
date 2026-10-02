//! Real retained-worker root-loss windows; Unix fixture only.
use super::*;

#[test]
fn operation_link_wal_actual_worker_ownership_loss_before_or_during_checkpoint_is_not_confirmed() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for during in [false, true] {
        let _profile =
            PersistenceProfileGuard::named(format!("link-maintenance-replacement-{during}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = guard.profile_root().to_path_buf();
        let displaced = temp.path().join(format!("displaced-{during}"));
        let counter = Arc::new(AtomicUsize::new(0));
        let setup = pressure_hook(counter.clone(), Fault::None);
        let hook_counter = counter.clone();
        let hook_root = root.clone();
        let hook_displaced = displaced.clone();
        let hook = Arc::new(move |connection: &Connection| {
            setup(connection);
            if during {
                let counter = hook_counter.clone();
                let root = hook_root.clone();
                let displaced = hook_displaced.clone();
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
            }
        });
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            hook,
            (!during).then(|| (ready.clone(), release.clone())),
        )
        .unwrap();
        available(&writer);
        let reply = writer
            .apply_operation_link(test_request(guard.owner_identity(), 1, 1, 7))
            .unwrap();
        if !during {
            ready.wait();
            std::fs::rename(&root, &displaced).unwrap();
            std::fs::create_dir(&root).unwrap();
            release.wait();
        }
        let result = reply.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(result, Err(LibraryError::ProfileOwnershipChanged { .. })),
            "replacement must not be confirmed current: {result:?}"
        );
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(during));
        assert!(!writer.shutdown());
        assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
        let readback = Connection::open_with_flags(
            displaced.join(LIBRARY_DB_FILE_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            readback
                .query_row("SELECT count(*) FROM operation_link_receipts", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
