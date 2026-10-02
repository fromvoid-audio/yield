use super::super::super::mutation_context::GlobalMutationRequest;

fn finish(writer: &mut GlobalLibraryWriter) -> GlobalLibraryWriterDrainReport {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(report) = writer.try_finish_shutdown() {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not finish bounded fixture"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

struct ReleaseGate(Option<Arc<std::sync::Barrier>>);
impl ReleaseGate {
    fn release(&mut self) {
        if let Some(gate) = self.0.take() {
            gate.wait();
        }
    }
}
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[test]
fn global_drain_pending_initialization_keeps_custody_and_cancels_before_sqlite_open() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempdir().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("drain-pending-initialization");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let (mut writer, gate) = GlobalLibraryWriter::start_with_initialization_gate_for_test(&guard);
    let mut release = ReleaseGate(Some(gate));
    let client = writer.profile_tag_vocabulary_client();
    let acquisition = writer.acquisition_id();
    assert!(writer.begin_shutdown());
    assert!(!writer.begin_shutdown());
    assert_eq!(writer.status(), GlobalLibraryWriterStatus::Closing);
    for _ in 0..32 {
        assert!(writer.try_finish_shutdown().is_none());
    }
    assert!(
        GlobalLibraryWriter::start(&guard).is_err(),
        "pending owner must keep its child lease"
    );
    assert!(matches!(
        client.lookup_profile_tag_categories(vec!["late".into()]),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert!(
        !guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME)
            .exists()
    );
    release.release();
    let report = finish(&mut writer);
    assert_eq!(report.acquisition_id(), &acquisition);
    assert_eq!(report.outcome(), &GlobalLibraryWriterDrainOutcome::Drained);
    assert_eq!(writer.status(), GlobalLibraryWriterStatus::Closed);
    assert_eq!(writer.shutdown_with_report(), report);
    assert!(!writer.shutdown());
    assert!(
        !guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME)
            .exists()
    );
    let mut replacement = GlobalLibraryWriter::start(&guard).unwrap();
    wait_until_available(&replacement);
    assert_eq!(
        replacement.shutdown_with_report().outcome(),
        &GlobalLibraryWriterDrainOutcome::Drained
    );
    assert_eq!(client.status(), GlobalLibraryWriterStatus::Closed);
}

#[cfg(unix)]
#[test]
fn global_drain_waits_for_accepted_post_commit_reply_and_never_reopens_admission() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempdir().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("drain-accepted-command");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let (mut writer, ready, gate) =
        GlobalLibraryWriter::start_with_post_command_gate_for_test(&guard);
    wait_until_available(&writer);
    let proposal = GlobalMutationRequest::registry(
        writer.acquisition_id(),
        1,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("source"), "drain-commit")]),
    )
    .unwrap();
    let context = proposal.context().clone();
    let reply = writer.submit_captured_mutation(proposal).unwrap();
    ready.wait();
    let mut release = ReleaseGate(Some(gate));
    writer.begin_shutdown();
    assert!(
        writer.try_finish_shutdown().is_none(),
        "committed SQL is not a completed drain before reply/close"
    );
    assert!(matches!(
        writer.load_source_registry_snapshot(),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    release.release();
    assert!(
        reply
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .into_result_for(&context)
            .is_ok()
    );
    assert_eq!(
        finish(&mut writer).outcome(),
        &GlobalLibraryWriterDrainOutcome::Drained
    );
    let observed = super::super::super::load_source_registry_snapshot_read_only(
        &guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME),
    )
    .unwrap();
    assert_eq!(observed.as_slice()[0].id.as_str(), "drain-commit");
}

#[test]
fn global_drain_worker_panic_is_sticky_unconfirmed_and_releases_owner_slot() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempdir().unwrap();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "drain-panic");
    wait_until_available(&writer);
    let client = writer.profile_tag_vocabulary_client();
    let receiver = writer.panic_owner_for_test().unwrap();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(2)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    let report = finish(&mut writer);
    assert_eq!(
        report.outcome(),
        &GlobalLibraryWriterDrainOutcome::WorkerPanicked
    );
    assert_eq!(
        writer.unavailable_reason(),
        Some(GlobalLibraryWriterUnavailable::WorkerPanicked)
    );
    assert_eq!(writer.shutdown_with_report(), report);
    assert_eq!(client.status(), GlobalLibraryWriterStatus::Closed);
    let mut replacement = GlobalLibraryWriter::start(&guard).unwrap();
    wait_until_available(&replacement);
    assert_eq!(
        replacement.shutdown_with_report().outcome(),
        &GlobalLibraryWriterDrainOutcome::Drained
    );
}

#[test]
fn global_drain_initialization_failure_retains_diagnostic_and_corrupt_bytes() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempdir().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("drain-unavailable");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let path = guard
        .profile_root()
        .join(super::super::super::LIBRARY_DB_FILE_NAME);
    let retained = b"not a SQLite database";
    fs::write(&path, retained).unwrap();
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while matches!(writer.status(), GlobalLibraryWriterStatus::Initializing) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let report = writer.shutdown_with_report();
    assert!(matches!(
        report.outcome(),
        GlobalLibraryWriterDrainOutcome::Unconfirmed(
            GlobalLibraryWriterUnavailable::DatabaseUnavailable { .. }
        )
    ));
    assert_eq!(writer.shutdown_with_report(), report);
    assert_eq!(fs::read(path).unwrap(), retained);
}

fn child(base: &Path, profile: &str, root: &Path, mode: &str, old_owner: &str) {
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sample_sources::library::owner::tests::shutdown_tests::global_drain_process_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("YIELD_CONFIG_HOME", base)
        .env("YIELD_CONFIG_PROFILE", profile)
        .env("YIELD_GLOBAL_DRAIN_TEST_ROOT", root)
        .env("YIELD_GLOBAL_DRAIN_TEST_MODE", mode)
        .env("YIELD_GLOBAL_DRAIN_TEST_OLD_OWNER", old_owner)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "bounded process fixture timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "child {mode}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn global_drain_two_process_ownership_readonly_and_owner_migration_are_compatible() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let _base = ConfigBaseGuard::set(base.clone());
    let profile = "global-drain-process";
    let _profile = PersistenceProfileGuard::named(profile);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = guard.profile_root().to_path_buf();
    let path = root.join(super::super::super::LIBRARY_DB_FILE_NAME);
    let legacy = Connection::open(&path).unwrap();
    legacy.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE sources(id TEXT PRIMARY KEY, root TEXT NOT NULL, sort_order INTEGER NOT NULL);").unwrap();
    legacy
        .execute(
            "INSERT INTO sources(id,root,sort_order) VALUES ('process-one',?1,0)",
            [base.to_str().unwrap()],
        )
        .unwrap();
    drop(legacy);
    let before = fs::read(&path).unwrap();
    assert!(super::super::super::load_source_registry_snapshot_read_only(&path).is_err());
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "read-only legacy observation must not migrate"
    );
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    wait_until_available(&writer);
    let migrated = writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(migrated.as_slice()[0].id.as_str(), "process-one");
    child(&base, profile, &root, "conflict", guard.owner_identity());
    let old_owner = guard.owner_identity().to_owned();
    assert_eq!(
        writer.shutdown_with_report().outcome(),
        &GlobalLibraryWriterDrainOutcome::Drained
    );
    drop(writer);
    drop(guard);
    child(&base, profile, &root, "owner", &old_owner);
}

#[test]
#[ignore = "bounded subprocess dispatched by global_drain_two_process_ownership_readonly_and_owner_migration_are_compatible"]
fn global_drain_process_helper() {
    let root = PathBuf::from(std::env::var_os("YIELD_GLOBAL_DRAIN_TEST_ROOT").unwrap());
    let path = root.join(super::super::super::LIBRARY_DB_FILE_NAME);
    match std::env::var("YIELD_GLOBAL_DRAIN_TEST_MODE")
        .unwrap()
        .as_str()
    {
        "conflict" => assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        )),
        "owner" => {
            let guard = WritableProfileGuard::acquire_current().unwrap();
            assert_eq!(guard.profile_root(), root);
            assert_ne!(
                guard.owner_identity(),
                std::env::var("YIELD_GLOBAL_DRAIN_TEST_OLD_OWNER").unwrap()
            );
            let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
            wait_until_available(&writer);
            assert_eq!(
                writer.shutdown_with_report().outcome(),
                &GlobalLibraryWriterDrainOutcome::Drained
            );
        }
        _ => panic!("invalid bounded fixture mode"),
    }
    let observed = super::super::super::load_source_registry_snapshot_read_only(&path).unwrap();
    assert_eq!(observed.as_slice().len(), 1);
    assert_eq!(observed.as_slice()[0].id.as_str(), "process-one");
}
