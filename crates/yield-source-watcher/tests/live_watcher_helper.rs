//! Run the actual helper executable with its private pipe protocol.

#![cfg(target_os = "macos")]

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use yield_source_watcher::macos_live::{HELPER_POLL, HELPER_READY_V1, decode_helper_hint};

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_yield-live-watch-helper");

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[track_caller]
fn response(stream: &mut impl Read, deadline: Instant) -> u8 {
    let mut byte = [0];
    loop {
        match stream.read(&mut byte) {
            Ok(1) => return byte[0],
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            outcome => panic!("missing exact helper response: {outcome:?}"),
        }
    }
}

#[test]
fn child_process_returns_native_hint_and_exits_when_requests_close() {
    let fixture = tempfile::tempdir().expect("disposable parent");
    let parent = fixture.path().canonicalize().expect("canonical parent");
    // Exercise the new executable's argument boundary before measuring stream readiness.
    assert_refusals(&parent.join("missing"));
    let root = parent.join("source");
    fs::create_dir(&root).expect("disposable source");
    let mut child = OwnedChild(
        Command::new(EXECUTABLE)
            .arg("--protocol-v1")
            .arg(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("launch actual watcher helper"),
    );
    let mut stdin = child.0.stdin.take().expect("private requests");
    let mut stdout = child.0.stdout.take().expect("private responses");
    // SAFETY: The pipe is open and solely owned by the test. Its reads have a finite deadline.
    assert_eq!(
        unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    assert_eq!(
        response(&mut stdout, Instant::now() + Duration::from_secs(2)),
        HELPER_READY_V1
    );
    fs::write(root.join("kick.wav"), b"disposable fixture").expect("write native descendant");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        stdin.write_all(&[HELPER_POLL]).expect("one pull");
        let evidence =
            decode_helper_hint(response(&mut stdout, deadline)).expect("bounded evidence");
        if evidence.has_descendant_change() {
            break;
        }
        assert!(Instant::now() < deadline, "missing native descendant hint");
        thread::sleep(Duration::from_millis(5));
    }
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(status) = child.0.try_wait().expect("reap helper") {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "watcher helper did not retire");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(stdout.read(&mut [0]).expect("responses closed"), 0);
}

fn assert_refusals(missing: &std::path::Path) {
    for (version, code) in [("--wrong-version", 64), ("--protocol-v1", 1)] {
        let result = Command::new(EXECUTABLE)
            .arg(version)
            .arg(&missing)
            .output()
            .expect("launch exact helper refusal");
        assert_eq!(result.status.code(), Some(code));
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
    }
}
