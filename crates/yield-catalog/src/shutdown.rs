use crate::CatalogService;
use std::thread::JoinHandle;

/// Completion of the original Catalog thread, without a Source settlement claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogThreadJoinOutcome {
    /// The original worker was joined without a panic.
    Joined,
    /// The original worker was joined after a panic.
    WorkerPanicked,
    /// No original handle or prior completed report was retained.
    MissingWorker,
}

/// Bounded original-thread completion evidence. This grants no retry or effect authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogThreadShutdownReport {
    outcome: CatalogThreadJoinOutcome,
    control_poisoned: bool,
}

impl CatalogThreadShutdownReport {
    /// Return the actual original-thread join outcome.
    pub fn outcome(self) -> CatalogThreadJoinOutcome {
        self.outcome
    }

    /// Whether an admission or join control mutex was poisoned before completion.
    pub fn control_poisoned(self) -> bool {
        self.control_poisoned
    }

    /// Whether the original join succeeded with unpoisoned control state.
    /// This does not confirm Source dispositions or durable settlement.
    pub fn is_confirmed_join(self) -> bool {
        self.outcome == CatalogThreadJoinOutcome::Joined && !self.control_poisoned
    }
}

pub(crate) struct WorkerJoinState {
    worker: Option<JoinHandle<()>>,
    completed: Option<CatalogThreadShutdownReport>,
}

impl WorkerJoinState {
    pub(crate) fn new(worker: Option<JoinHandle<()>>) -> Self {
        Self {
            worker,
            completed: None,
        }
    }
}

impl CatalogService {
    /// Request cancellation and synchronously join the original worker.
    pub fn shutdown(&self) {
        let _ = self.shutdown_with_report();
    }

    /// Join the original worker and retain its bounded completion report.
    /// Concurrent callers wait for that same join; repeated callers receive its cached result.
    /// Poisoned control state remains a failure, but never skips the original join.
    pub fn shutdown_with_report(&self) -> CatalogThreadShutdownReport {
        let mut control_poisoned = false;
        {
            let mut admission = self.admission.lock().unwrap_or_else(|poisoned| {
                control_poisoned = true;
                poisoned.into_inner()
            });
            if !admission.shutdown && !admission.closed {
                admission.shutdown = true;
                admission.invalidate_all();
            }
        }
        // Hold the original handle mutex through join and cache publication. A taken handle
        // is never mistaken for a completed join by another caller.
        let mut state = self.worker.lock().unwrap_or_else(|poisoned| {
            control_poisoned = true;
            poisoned.into_inner()
        });
        if let Some(report) = state.completed {
            return report;
        }
        let outcome = match state.worker.take() {
            Some(worker) => match worker.join() {
                Ok(()) => CatalogThreadJoinOutcome::Joined,
                Err(_) => CatalogThreadJoinOutcome::WorkerPanicked,
            },
            None => CatalogThreadJoinOutcome::MissingWorker,
        };
        let report = CatalogThreadShutdownReport {
            outcome,
            control_poisoned,
        };
        state.completed = Some(report);
        report
    }
}

#[cfg(test)]
mod tests;
