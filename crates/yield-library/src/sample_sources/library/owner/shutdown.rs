//! Typed observations of this existing owner's teardown; no durable recovery authority.
use super::*;

/// Terminal result after the owner thread is joined and its retained resources close.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlobalLibraryWriterDrainOutcome {
    /// Accepted commands reached terminal replies and the owner closed normally.
    /// Individual command errors and lost replies remain their own outcomes.
    Drained,
    /// Initialization/ownership failure prevented confirmation of a normal drain.
    Unconfirmed(GlobalLibraryWriterUnavailable),
    /// The worker unwound; accepted effects/results may be unconfirmed.
    WorkerPanicked,
}

/// Acquisition-bound teardown observation. This is not a journal checkpoint or file grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalLibraryWriterDrainReport {
    acquisition: ProfileAcquisitionId,
    outcome: GlobalLibraryWriterDrainOutcome,
}
impl GlobalLibraryWriterDrainReport {
    /// Original owner acquisition, never a fresh replacement identity.
    pub fn acquisition_id(&self) -> &ProfileAcquisitionId {
        &self.acquisition
    }
    /// Terminal drain diagnostic; it does not assert that every command succeeded.
    pub fn outcome(&self) -> &GlobalLibraryWriterDrainOutcome {
        &self.outcome
    }
}

impl GlobalLibraryWriter {
    /// Close admission without joining or waiting for queued SQL/initialization.
    ///
    /// The existing owner retains its profile/child lease and processes the finite accepted
    /// queue. Call `try_finish_shutdown` to observe completion. This never detaches a writer.
    /// Returns whether this call initiated closure, for compatibility with `shutdown`.
    pub fn begin_shutdown(&mut self) -> bool {
        let _admission = self
            .admission_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous =
            WriterLifecycle::from_u8(self.lifecycle.load(std::sync::atomic::Ordering::Acquire));
        if !matches!(previous, WriterLifecycle::Closing | WriterLifecycle::Closed) {
            self.lifecycle.store(
                WriterLifecycle::Closing as u8,
                std::sync::atomic::Ordering::Release,
            );
        }
        self.commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        #[cfg(test)]
        self.blocked_receiver.take();
        !matches!(previous, WriterLifecycle::Closing | WriterLifecycle::Closed)
    }

    /// Close admission and return pending while the owner is executing accepted work.
    ///
    /// Joins only after `JoinHandle::is_finished`; there is no polling/drain loop here.
    /// OS scheduling/thread-local teardown can still delay a finished thread's join. This
    /// is a cooperative observation API, not a hard realtime latency guarantee.
    pub fn try_finish_shutdown(&mut self) -> Option<GlobalLibraryWriterDrainReport> {
        self.begin_shutdown();
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return None;
        }
        Some(self.finish_shutdown())
    }

    /// Close admission, synchronously join the existing owner, and retain its exact result.
    ///
    /// Call from a teardown worker when blocking is acceptable. For a pending owner use the
    /// separate begin/poll API; a deadline expiry is pending, not successful teardown.
    pub fn shutdown_with_report(&mut self) -> GlobalLibraryWriterDrainReport {
        self.begin_shutdown();
        self.finish_shutdown()
    }

    pub(super) fn finish_shutdown(&mut self) -> GlobalLibraryWriterDrainReport {
        if let Some(report) = &self.drain_report {
            return report.clone();
        }
        let panicked = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        let outcome = if panicked {
            GlobalLibraryWriterDrainOutcome::WorkerPanicked
        } else if let Some(reason) = self.unavailable_reason() {
            GlobalLibraryWriterDrainOutcome::Unconfirmed(reason)
        } else {
            GlobalLibraryWriterDrainOutcome::Drained
        };
        self.lifecycle.store(
            WriterLifecycle::Closed as u8,
            std::sync::atomic::Ordering::Release,
        );
        let report = GlobalLibraryWriterDrainReport {
            acquisition: self.acquisition.clone(),
            outcome,
        };
        self.drain_report = Some(report.clone());
        report
    }
}

// Ensure a panic cannot leave cloneable clients claiming that the exited worker is available.
pub(super) struct OwnerExitStatus {
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
}
impl OwnerExitStatus {
    pub(super) fn new(
        lifecycle: Arc<std::sync::atomic::AtomicU8>,
        unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    ) -> Self {
        Self {
            lifecycle,
            unavailable,
        }
    }
}
impl Drop for OwnerExitStatus {
    fn drop(&mut self) {
        if std::thread::panicking() {
            *self.unavailable.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(GlobalLibraryWriterUnavailable::WorkerPanicked);
            self.lifecycle.store(
                WriterLifecycle::Closed as u8,
                std::sync::atomic::Ordering::Release,
            );
        }
    }
}
