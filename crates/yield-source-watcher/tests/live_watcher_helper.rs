//! Run the actual helper executable with its private pipe protocol.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use yield_source_watcher::macos_live::MacosWatcherProcess;

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_yield-live-watch-helper");

#[test]
fn child_process_returns_native_hint_and_exits_when_requests_close() {
    let fixture = tempfile::tempdir().expect("disposable parent");
    let parent = fixture.path().canonicalize().expect("canonical parent");
    // Exercise the new executable's argument boundary before measuring stream readiness.
    assert_refusals(&parent.join("missing"));
    let root = parent.join("source");
    fs::create_dir(&root).expect("disposable source");
    let mut child = MacosWatcherProcess::spawn(
        Path::new(EXECUTABLE),
        &root,
        Instant::now() + Duration::from_secs(2),
    )
    .expect("launch actual watcher helper");
    while !child.poll_ready().expect("checked native readiness") {
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(root.join("kick.wav"), b"disposable fixture").expect("write native descendant");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if child
            .poll_hint()
            .expect("bounded helper evidence")
            .is_some_and(|evidence| evidence.has_descendant_change())
        {
            break;
        }
        assert!(Instant::now() < deadline, "missing native descendant hint");
        thread::sleep(Duration::from_millis(5));
    }
    child.begin_retirement();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(status) = child.try_reap().expect("reap helper") {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "watcher helper did not retire");
        thread::sleep(Duration::from_millis(5));
    }
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
