//! Fixed idle pacing on the original worker, with short shutdown admission only.
use super::{QueuedCommand, WriterLifecycle};
use std::sync::{
    Mutex,
    atomic::{AtomicU8, Ordering},
    mpsc::{Receiver, TryRecvError},
};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(15);

pub(super) struct Pacing {
    since: Instant,
}
const _: () = assert!(std::mem::size_of::<Pacing>() <= 32);

pub(super) enum Poll {
    Command(QueuedCommand),
    Maintain,
    Wait,
    Disconnected,
}

impl Pacing {
    pub(super) fn new(now: Instant) -> Self {
        Self { since: now }
    }
    pub(super) fn activity(&mut self, now: Instant) {
        self.since = now;
    }
    pub(super) fn poll(
        &mut self,
        now: Instant,
        commands: &Receiver<QueuedCommand>,
        lifecycle: &AtomicU8,
        admission: &Mutex<()>,
    ) -> Poll {
        if now.saturating_duration_since(self.since) < INTERVAL {
            return Poll::Wait;
        }
        // No SQLite, filesystem work or owner wait under this gate. Closing competes
        // with admission; an already admitted pass retains its original resources.
        let _admission = admission.lock().unwrap_or_else(|p| p.into_inner());
        if WriterLifecycle::from_u8(lifecycle.load(Ordering::Acquire)) != WriterLifecycle::Available
        {
            return Poll::Wait;
        }
        // A command may have arrived after recv_timeout. Prefer it under the same
        // submission gate; do not add idle work ahead of an already queued command.
        match commands.try_recv() {
            Ok(command) => Poll::Command(command),
            Err(TryRecvError::Disconnected) => Poll::Disconnected,
            Err(TryRecvError::Empty) => {
                self.activity(now);
                Poll::Maintain
            }
        }
    }
}

#[cfg(test)]
mod tests;
