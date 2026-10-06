// Immediate close/reacquire assertions require a process with no unrelated fork callers.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const CHILD_CASE: &str = "YIELD_VOLUME_RESTART_ISOLATED_CASE";
const CHILD_MARKER: &str = "YIELD_VOLUME_RESTART_ISOLATED_MARKER";

struct IsolatedChild(std::process::Child);
impl Drop for IsolatedChild {
    fn drop(&mut self) {
        // Also clean up if status polling or an assertion panics.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Execute the original test body once, without other libtest cases that can fork while its
/// flock descriptor is open. This changes fixture scheduling, never acquisition/retry policy.
pub(in crate::operation_journal::capacity_volume_owner) fn run_in_child(case: &str) -> bool {
    if std::env::var(CHILD_CASE).as_deref() == Ok(case) {
        fs::write(std::env::var_os(CHILD_MARKER).expect("child marker"), case)
            .expect("record exact test body entry");
        return false;
    }
    let marker_directory = tempfile::tempdir().expect("private child marker directory");
    let marker = marker_directory.path().join("entered");
    let module = if case == "fork_retains_actual_owner_lock_until_exec" {
        "operation_journal::capacity_volume_owner::tests::process_isolation"
    } else if case
        == "original_sealed_bootstrap_retains_exact_lock_and_registry_at_each_refused_prefix"
    {
        "operation_journal::capacity_volume_owner::legacy_bootstrap::tests::native"
    } else {
        "operation_journal::capacity_volume_owner::tests"
    };
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    let mut child = IsolatedChild(
        command
            .args([
                "--exact",
                &format!("{module}::{case}"),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD_CASE, case)
            .env(CHILD_MARKER, &marker)
            .spawn()
            .expect("spawn isolated restart case"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.0.try_wait().expect("poll isolated case") {
            Some(status) => {
                assert!(status.success(), "isolated {case} failed: {status}");
                assert_eq!(
                    fs::read_to_string(&marker).expect("exact test body was not executed"),
                    case,
                    "child must execute the requested original body, not zero selected tests"
                );
                return true;
            }
            None if Instant::now() >= deadline => {
                panic!("isolated {case} exceeded 30 seconds");
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

#[test]
#[should_panic(expected = "exact test body was not executed")]
fn isolation_rejects_zero_selected_tests() {
    run_in_child("nonexistent_volume_restart_case");
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    // OwnedFd closes both endpoints on all parent paths; CLOEXEC is the behavior under test.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

struct ForkChild(libc::pid_t);
impl ForkChild {
    fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.0, &mut status, libc::WNOHANG) };
            if result == self.0 {
                self.0 = 0;
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                return;
            }
            assert!(
                result >= 0,
                "waitpid failed: {}",
                io::Error::last_os_error()
            );
            assert!(Instant::now() < deadline, "fork child did not exec/exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for ForkChild {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                libc::waitpid(self.0, std::ptr::null_mut(), 0);
            }
        }
    }
}

#[test]
fn fork_retains_actual_owner_lock_until_exec() {
    if run_in_child("fork_retains_actual_owner_lock_until_exec") {
        return;
    }
    let fixture = fixture();
    let mut first = acquire_fixture_for_test(
        &fixture.descriptor,
        &fixture.registry,
        volume(49),
        "profile-one",
    )
    .expect("actual volume owner");
    first.mark_clean_after_handoff().expect("clean handoff");
    let retained = first.try_clone_for_context().expect("retained context");
    drop(first);
    assert!(
        matches!(
            acquire_fixture_for_test(
                &fixture.descriptor,
                &fixture.registry,
                volume(49),
                "profile-one"
            ),
            Err(VolumeOwnershipError::OwnedByAnotherProcess { .. })
        ),
        "dropping one context must not unlock another retained context"
    );
    assert!(matches!(
        retained.revalidate(),
        Err(VolumeOwnershipError::HandoffFenced { .. })
    ));
    let descriptor_flags = unsafe { libc::fcntl(retained.lock_file.as_raw_fd(), libc::F_GETFD) };
    assert!(descriptor_flags >= 0, "read retained descriptor flags");
    assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
    let (go_read, go_write) = pipe();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
    if pid == 0 {
        // After fork only async-signal-safe libc calls: no allocation, Rust IO, locks or drops.
        // The child inherits the actual retained capability's flock open-file description.
        unsafe {
            libc::close(go_write.as_raw_fd());
            let mut poll = libc::pollfd {
                fd: go_read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let mut byte = 0u8;
            if libc::poll(&mut poll, 1, 10_000) != 1
                || libc::read(go_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) != 1
            {
                libc::_exit(91);
            }
            let argv = [c"true".as_ptr(), std::ptr::null()];
            libc::execv(c"/bin/true".as_ptr(), argv.as_ptr());
            libc::_exit(92);
        }
    }
    let mut child = ForkChild(pid);
    drop(go_read);
    drop(retained);
    assert!(
        matches!(
            acquire_fixture_for_test(
                &fixture.descriptor,
                &fixture.registry,
                volume(49),
                "profile-one"
            ),
            Err(VolumeOwnershipError::OwnedByAnotherProcess { .. })
        ),
        "parked fork child retains the lock even after every parent owner closes"
    );
    assert_eq!(
        unsafe { libc::write(go_write.as_raw_fd(), b"x".as_ptr().cast(), 1) },
        1
    );
    child.wait();
    let restarted = acquire_fixture_for_test(
        &fixture.descriptor,
        &fixture.registry,
        volume(49),
        "profile-one",
    )
    .expect("one acquisition after child exec closed CLOEXEC descriptors");
    assert_eq!(restarted.owner_generation(), 2);
}
