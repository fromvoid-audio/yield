//! One fixed idle opportunity per worker, without a growing source cursor inventory.
use super::{
    AdmissionState, QueuedCatalogCommand, RegisteredSource, SourceId, publication_is_current_state,
};
use std::collections::HashMap;
use std::sync::{
    Mutex,
    mpsc::{Receiver, TryRecvError},
};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(15);

pub(super) struct Pacing {
    since: Instant,
    next: usize,
}
const _: () = assert!(std::mem::size_of::<Pacing>() <= 48);

pub(super) enum Poll {
    Command(QueuedCatalogCommand),
    Source(usize),
    Wait,
    Disconnected,
}

impl Pacing {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            since: now,
            next: 0,
        }
    }
    pub(super) fn activity(&mut self, now: Instant) {
        self.since = now;
    }
    pub(super) fn poll(
        &mut self,
        now: Instant,
        commands: &Receiver<QueuedCatalogCommand>,
        admission: &Mutex<AdmissionState>,
        sources: &HashMap<SourceId, RegisteredSource>,
        parked: bool,
    ) -> Poll {
        if parked {
            self.activity(now);
            return Poll::Wait;
        }
        if now.saturating_duration_since(self.since) < INTERVAL {
            return Poll::Wait;
        }
        // Only scalar lifecycle/queue checks under the existing submission gate. Native work,
        // SQLite and source owner waits occur after this guard is dropped on return.
        let state = admission.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutdown || state.closed {
            return Poll::Wait;
        }
        match commands.try_recv() {
            Ok(command) => return Poll::Command(command),
            Err(TryRecvError::Disconnected) => return Poll::Disconnected,
            Err(TryRecvError::Empty) => {}
        }
        if state.queued_commands != 0 {
            return Poll::Wait;
        }
        self.activity(now);
        if sources.is_empty() {
            self.next = 0;
            return Poll::Wait;
        }
        let index = self.next % sources.len();
        self.next = index.checked_add(1).unwrap_or(0);
        // Stable idle maps rotate through every original entry. Membership changes can change
        // order, but do not retain source IDs, allocate cursors or admit more than one pass.
        let (id, source) = sources.iter().nth(index).expect("bounded original index");
        if publication_is_current_state(&state, id, source.lifecycle_generation, None) {
            Poll::Source(index)
        } else {
            Poll::Wait
        }
    }
}

#[cfg(test)]
#[path = "tests/idle_maintenance.rs"]
mod tests;
