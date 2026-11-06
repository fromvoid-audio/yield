//! Run the actual helper executable with its private pipe protocol.

#![cfg(target_os = "macos")]

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use yield_source_watcher::macos_live::{HELPER_READY_V1, MacosWatcherProcess};

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
