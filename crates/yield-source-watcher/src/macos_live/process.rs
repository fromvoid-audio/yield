//! Nonblocking parent-side transport with original child process custody.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Instant;

use super::{HELPER_POLL, HELPER_READY_V1, WatchHintEvidence, decode_helper_hint};

enum Phase {
    Starting { deadline: Instant },
    Active { awaiting_reply: bool },
    Retiring,
}

/// One original watcher child and its nonblocking private channel.
///
/// Keep this owner through retirement until [reap](Self::try_reap) reports an exit. Dropping an
/// unreaped child kills and waits as a last resort and can block. Source identity, generation and
/// the choice of trusted executable belong to the caller.
pub struct MacosWatcherProcess {
    child: Child,
    channel: Option<UnixStream>,
    phase: Phase,
}

impl MacosWatcherProcess {
    /// Spawn the already selected helper for one canonical root and absolute readiness deadline.
    ///
    /// OS process creation is synchronous. Readiness is polled separately and the deadline
    /// includes any time spent in process creation.
    pub fn spawn(executable: &Path, root: &Path, deadline: Instant) -> io::Result<Self> {
        if !executable.is_absolute() || !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "watcher executable and root must use absolute paths",
            ));
        }
        let mut command = Command::new(executable);
        command.arg("--protocol-v1").arg(root);
        Self::spawn_command(command, deadline)
    }

    fn spawn_command(mut command: Command, deadline: Instant) -> io::Result<Self> {
        if Instant::now() >= deadline {
            return Err(readiness_timeout());
        }
        let (parent, child) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let input = OwnedFd::from(child.try_clone()?);
        let output = OwnedFd::from(child);
        let child = command
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .spawn()?;
        Ok(Self {
            child,
            channel: Some(parent),
            phase: Phase::Starting { deadline },
        })
    }

    /// Poll checked readiness without waiting for the native backend.
    ///
    /// Pending returns false. Failure closes the request channel and leaves the child owned.
    pub fn poll_ready(&mut self) -> io::Result<bool> {
        let result = self.poll_ready_inner();
        if result.is_err() {
            self.begin_retirement();
        }
        result
    }

    fn poll_ready_inner(&mut self) -> io::Result<bool> {
        let Phase::Starting { deadline } = self.phase else {
            return if matches!(self.phase, Phase::Active { .. }) {
                Ok(true)
            } else {
                Err(closed())
            };
        };
        if Instant::now() >= deadline {
            return Err(readiness_timeout());
        }
        let byte = read_byte(self.channel.as_mut().ok_or_else(closed)?)?;
        if Instant::now() >= deadline {
            return Err(readiness_timeout());
        }
        match byte {
            Some(HELPER_READY_V1) => {
                self.phase = Phase::Active {
                    awaiting_reply: false,
                };
                Ok(true)
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid macOS watcher readiness byte",
            )),
            None => Ok(false),
        }
    }

    /// Poll a single coalesced hint. At most one request is outstanding.
    ///
    /// None means the channel would block. An empty evidence value is a complete response.
    /// Failure closes the request channel and leaves the child owned.
    pub fn poll_hint(&mut self) -> io::Result<Option<WatchHintEvidence>> {
        let result = self.poll_hint_inner();
        if result.is_err() {
            self.begin_retirement();
        }
        result
    }

    fn poll_hint_inner(&mut self) -> io::Result<Option<WatchHintEvidence>> {
        let Phase::Active { awaiting_reply } = &mut self.phase else {
            return Err(closed());
        };
        let channel = self.channel.as_mut().ok_or_else(closed)?;
        if !*awaiting_reply {
            match channel.write(&[HELPER_POLL]) {
                Ok(1) => *awaiting_reply = true,
                Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
                Err(error) if would_block(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let Some(byte) = read_byte(channel)? else {
            return Ok(None);
        };
        *awaiting_reply = false;
        decode_helper_hint(byte).map(Some).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid macOS watcher hint byte",
            )
        })
    }

    /// Close the protocol channel. The child remains owned until reaped.
    pub fn begin_retirement(&mut self) {
        self.phase = Phase::Retiring;
        self.channel.take();
    }

    /// Signal termination of the original child without waiting for it to exit.
    pub fn force_termination(&mut self) -> io::Result<()> {
        self.begin_retirement();
        self.child.kill()
    }

    /// Observe and reap the original child if it has exited, without waiting.
    pub fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.begin_retirement();
        }
        Ok(status)
    }
}

impl Drop for MacosWatcherProcess {
    fn drop(&mut self) {
        self.begin_retirement();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_byte(channel: &mut UnixStream) -> io::Result<Option<u8>> {
    let mut byte = [0];
    match channel.read(&mut byte) {
        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(_) => Ok(Some(byte[0])),
        Err(error) if would_block(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn would_block(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn readiness_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "macOS watcher readiness expired")
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "macOS watcher channel closed")
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn deadline_keeps_stopped_original_child_until_termination_and_reap() {
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut owner = MacosWatcherProcess::spawn_command(shell("kill -STOP $$"), deadline)
            .expect("spawn stopped-child fixture");
        let pid = owner.child.id();
        let error = loop {
            match owner.poll_ready() {
                Ok(false) => thread::sleep(Duration::from_millis(2)),
                Ok(true) => panic!("stopped child reached ready"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(owner.child.id(), pid);
        assert!(owner.try_reap().expect("poll original child").is_none());
        owner.force_termination().expect("signal original child");
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(exit) = owner.try_reap().expect("reap original child") {
                assert_eq!(exit.signal(), Some(libc::SIGKILL));
                break;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn rejects_malformed_startup_and_early_exit() {
        for (script, expected) in [
            ("printf 'x'", io::ErrorKind::InvalidData),
            ("exit 0", io::ErrorKind::UnexpectedEof),
        ] {
            let deadline = Instant::now() + Duration::from_millis(600);
            let mut owner = MacosWatcherProcess::spawn_command(shell(script), deadline)
                .expect("spawn malformed child fixture");
            loop {
                match owner.poll_ready() {
                    Ok(false) => thread::sleep(Duration::from_millis(2)),
                    Ok(true) => panic!("invalid child reached ready"),
                    Err(error) => {
                        assert_eq!(error.kind(), expected);
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn sends_only_one_pull_before_the_pending_reply() {
        let (channel, mut peer) = UnixStream::pair().expect("private protocol fixture");
        channel.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let child = shell("kill -STOP $$").spawn().expect("owned fixture child");
        let mut owner = MacosWatcherProcess {
            child,
            channel: Some(channel),
            phase: Phase::Active {
                awaiting_reply: false,
            },
        };
        for _ in 0..3 {
            assert_eq!(owner.poll_hint().unwrap(), None);
        }
        let mut requests = [0; 3];
        assert_eq!(peer.read(&mut requests).unwrap(), 1);
        assert_eq!(requests[0], HELPER_POLL);
        assert_eq!(
            peer.read(&mut requests).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        peer.write_all(&[0]).unwrap();
        assert_eq!(owner.poll_hint().unwrap(), Some(WatchHintEvidence::EMPTY));
    }
}
