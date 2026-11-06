//! Observe the original parent independently of the request protocol.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

const STOP_IDENT: usize = 1;

pub(super) struct ParentLifetime {
    queue: Arc<OwnedFd>,
    monitor: Option<JoinHandle<()>>,
}

impl ParentLifetime {
    pub(super) fn bind() -> io::Result<Self> {
        // SAFETY: getppid takes no arguments, and kqueue returns a newly owned descriptor.
        let parent = unsafe { libc::getppid() };
        if parent <= 1 {
            return Err(parent_lost());
        }
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: A successful kqueue call returned an owned descriptor.
        let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        let events = [
            event(
                parent as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_ONESHOT,
                libc::NOTE_EXIT,
            ),
            event(STOP_IDENT, libc::EVFILT_USER, libc::EV_ADD, 0),
        ];
        submit(&queue, &events)?;
        // SAFETY: Recheck reparenting after registration, including a death during setup.
        if unsafe { libc::getppid() } != parent {
            return Err(parent_lost());
        }
        let monitor_queue = Arc::clone(&queue);
        let monitor = thread::Builder::new()
            .name("yield-live-watch-parent".into())
            .spawn(move || monitor_parent(&monitor_queue))?;
        Ok(Self {
            queue,
            monitor: Some(monitor),
        })
    }
}

impl Drop for ParentLifetime {
    fn drop(&mut self) {
        let Some(monitor) = self.monitor.take() else {
            return;
        };
        let stop = event(STOP_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        if submit(&self.queue, &[stop]).is_err() || monitor.join().is_err() {
            terminate_helper();
        }
    }
}

fn monitor_parent(queue: &OwnedFd) {
    loop {
        let mut received = MaybeUninit::<libc::kevent>::uninit();
        // SAFETY: The queue remains owned and kevent initializes at most one output record.
        let count = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                ptr::null(),
                0,
                received.as_mut_ptr(),
                1,
                ptr::null(),
            )
        };
        if count == 1 {
            // SAFETY: A count of one confirms that the output record was written.
            let event = unsafe { received.assume_init() };
            if event.filter == libc::EVFILT_USER && event.flags & libc::EV_ERROR == 0 {
                return;
            }
            // This process only holds read-only watcher resources. Process exit retires them
            // even if its main thread is still waiting on the native registration call.
            terminate_helper();
        }
        if count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        terminate_helper();
    }
}

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: ptr::null_mut(),
    }
}

fn submit(queue: &OwnedFd, events: &[libc::kevent]) -> io::Result<()> {
    // SAFETY: The queue is owned, and the initialized change list remains live for the call.
    let status = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            events.as_ptr(),
            events.len() as libc::c_int,
            ptr::null_mut(),
            0,
            ptr::null(),
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn parent_lost() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "original watcher parent exited")
}

fn terminate_helper() -> ! {
    // SAFETY: This private read-only process has no state to flush. Exit without running
    // handlers that could wait on a thread blocked in the native watcher call.
    unsafe { libc::_exit(70) }
}
