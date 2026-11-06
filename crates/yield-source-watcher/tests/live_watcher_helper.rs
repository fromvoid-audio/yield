//! Run the actual helper executable with its private pipe protocol.

#![cfg(target_os = "macos")]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use yield_library::filesystem_identity::stable_filesystem_identity;
use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
use yield_source_watcher::SourceWatchSpec;
use yield_source_watcher::macos_live::{
    HELPER_READY_V1, MacosSourceWatcherSlot, WatcherSlotProgress,
};

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_yield-live-watch-helper");

#[test]
fn child_process_returns_native_hint_and_exits_when_requests_close() {
    let fixture = tempfile::tempdir().expect("disposable parent");
    let parent = fixture.path().canonicalize().expect("canonical parent");
    // Exercise the new executable's argument boundary before measuring stream readiness.
    assert_refusals(&parent.join("missing"));
    let root = parent.join("source");
    fs::create_dir(&root).expect("disposable source");
    let metadata = fs::metadata(&root).expect("source metadata");
    let identity = stable_filesystem_identity(&root, &metadata).expect("original source identity");
    let original = SourceWatchSpec::new(
        SourceId::from_string("bound-native-helper"),
        7,
        root.clone(),
        Some(RootIdentity::from_bytes(identity.into_bytes())),
    );
    let mut child = MacosSourceWatcherSlot::start(
        Path::new(EXECUTABLE),
        original.clone(),
        Instant::now() + Duration::from_secs(2),
    )
    .expect("launch actual watcher helper");
    let watcher = loop {
        match child
            .poll(Some(&original))
            .expect("checked native readiness")
        {
            WatcherSlotProgress::Ready(watcher) => break watcher,
            WatcherSlotProgress::Pending => thread::sleep(Duration::from_millis(5)),
            _ => panic!("unexpected watcher slot state"),
        }
    };
    fs::write(root.join("kick.wav"), b"disposable fixture").expect("write native descendant");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        assert!(matches!(
            child
                .poll(Some(&original))
                .expect("bounded helper evidence"),
            WatcherSlotProgress::Active
        ));
        if let Some(hint) = watcher.take_hint()
            && hint.evidence().has_descendant_change()
        {
            assert_eq!(hint.source_id(), original.source_id());
            assert_eq!(hint.lifecycle_generation(), 7);
            assert_eq!(hint.root_identity(), original.root_identity());
            break;
        }
        assert!(Instant::now() < deadline, "missing native descendant hint");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        child.poll(None).unwrap(),
        WatcherSlotProgress::Retiring
    ));
    assert!(watcher.is_closed());
    assert!(reap(&mut child).success());
}

fn spec(root: PathBuf, generation: u64) -> SourceWatchSpec {
    SourceWatchSpec::new(
        SourceId::from_string("bound-native-helper"),
        generation,
        root,
        None,
    )
}

fn reap(child: &mut MacosSourceWatcherSlot) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(status) = child.try_reap().expect("reap helper") {
            return status;
        }
        assert!(Instant::now() < deadline, "watcher helper did not retire");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn late_readiness_cannot_admit_a_replacement_or_superseded_source() {
    let fixture = tempfile::tempdir().expect("disposable parent");
    let parent = fixture.path().canonicalize().expect("canonical parent");
    let root = parent.join("source");
    fs::create_dir(&root).unwrap();
    let original = spec(root.clone(), 7);
    let mut child = MacosSourceWatcherSlot::start(
        Path::new(EXECUTABLE),
        original.clone(),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let replacement = spec(root.clone(), 8);
    assert!(matches!(
        child.poll(Some(&replacement)).unwrap(),
        WatcherSlotProgress::Retiring
    ));
    let _original_status = reap(&mut child);

    let mut child = MacosSourceWatcherSlot::start(
        Path::new(EXECUTABLE),
        original.clone(),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    fs::rename(&root, parent.join("displaced")).unwrap();
    fs::create_dir(&root).unwrap();
    loop {
        match child.poll(Some(&original)) {
            Ok(WatcherSlotProgress::Pending) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                assert!(
                    error.kind() == io::ErrorKind::UnexpectedEof
                        || error.to_string() == "source root changed before watcher admission",
                    "{error:?}"
                );
                break;
            }
            Ok(_) => panic!("replacement root became eligible"),
        }
    }
    assert!(matches!(
        child.poll(Some(&original)).unwrap(),
        WatcherSlotProgress::Retiring
    ));
    let _original_status = reap(&mut child);
}

fn assert_refusals(missing: &Path) {
    for (version, code) in [("--wrong-version", 64), ("--protocol-v1", 1)] {
        let result = Command::new(EXECUTABLE)
            .arg(version)
            .arg(missing)
            .output()
            .expect("launch exact helper refusal");
        assert_eq!(result.status.code(), Some(code));
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
    }
}

struct FixtureParent(Child);

impl Drop for FixtureParent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn actual_helper_exits_on_original_parent_death_with_requests_still_open() {
    let fixture = tempfile::tempdir().expect("disposable parent");
    let directory = fixture.path().canonicalize().expect("canonical fixture");
    let root = directory.join("source");
    let ready = directory.join("ready");
    let pid = directory.join("child-pid");
    fs::create_dir(&root).expect("disposable source");
    // Retain the grandparent's write end and pass the original read end through
    // the intermediate parent. Its death cannot itself produce request EOF.
    let mut parent = FixtureParent(
        Command::new("/bin/sh")
            .args([
                "-c",
                "exec 3<&0; \"$1\" --protocol-v1 \"$2\" <&3 >\"$3\" & child=$!; echo \"$child\" >\"$4\"; wait \"$child\"",
                "watcher-fixture-parent",
                EXECUTABLE,
            ])
            .arg(&root)
            .arg(&ready)
            .arg(&pid)
            .stdin(Stdio::piped())
            .spawn()
            .expect("launch disposable original parent"),
    );
    let requests = parent.0.stdin.take().expect("retain requests");
    let deadline = Instant::now() + Duration::from_secs(2);
    let child_pid: libc::pid_t = loop {
        if fs::read(&ready).is_ok_and(|bytes| bytes == [HELPER_READY_V1])
            && let Ok(text) = fs::read_to_string(&pid)
            && let Ok(pid) = text.trim().parse()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "watcher never reached readiness");
        thread::sleep(Duration::from_millis(5));
    };
    assert!(child_pid > 1);
    // SAFETY: Signal zero queries existence without sending a signal.
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, 0);
    parent
        .0
        .kill()
        .expect("terminate the original fixture parent");
    parent.0.wait().expect("reap the fixture parent");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // SAFETY: Signal zero queries existence without affecting the helper or any process.
        let gone = unsafe { libc::kill(child_pid, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            break;
        }
        assert!(Instant::now() < deadline, "orphan watcher did not exit");
        thread::sleep(Duration::from_millis(5));
    }
    drop(requests);
}
