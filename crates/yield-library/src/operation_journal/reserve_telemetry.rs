//! Fixed process-wide observations of existing protected-reserve namespace entrypoints.
//!
//! Counts and tracing categories contain no profile/root/volume/operation identity, raw
//! descriptor observations or error details. They confer no reserve or retry authority.
use std::sync::atomic::{AtomicU64, Ordering};

use super::capacity_reserve_namespace::{ReserveNamespaceFailure, ReserveNamespaceFailureStage};

/// Existing reserve namespace entrypoint whose return was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum ReserveAttempt {
    /// Create and bind the fixed namespace child.
    Create,
    /// Reacquire the complete retained reserve extent.
    Reopen,
    /// Reacquire an extent after a durable spend observation.
    ReopenPartial,
}
impl ReserveAttempt {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Reopen => "reopen",
            Self::ReopenPartial => "reopen_partial",
        }
    }
}

/// Fixed result category; no error text or binding facts escape this observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum ReserveDecision {
    /// The existing entrypoint returned a bound reserve; no new qualification is inferred.
    Bound,
    /// The platform has no supported namespace implementation.
    Unsupported,
    /// Retained root validation failed.
    RootValidation,
    /// Namespace child opening failed.
    Open,
    /// Allocation failed.
    Allocation,
    /// Namespace synchronization failed.
    Sync,
    /// Binding the retained descriptor to the namespace failed.
    Binding,
    /// Descriptor or namespace observation failed.
    Observation,
}
impl ReserveDecision {
    fn label(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Unsupported => "unsupported",
            Self::RootValidation => "root_validation",
            Self::Open => "open",
            Self::Allocation => "allocation",
            Self::Sync => "sync",
            Self::Binding => "binding",
            Self::Observation => "observation",
        }
    }
    fn from_failure(stage: ReserveNamespaceFailureStage) -> Self {
        match stage {
            ReserveNamespaceFailureStage::Unsupported => Self::Unsupported,
            ReserveNamespaceFailureStage::RootValidation => Self::RootValidation,
            ReserveNamespaceFailureStage::Open => Self::Open,
            ReserveNamespaceFailureStage::Allocation => Self::Allocation,
            ReserveNamespaceFailureStage::Sync => Self::Sync,
            ReserveNamespaceFailureStage::Binding => Self::Binding,
            ReserveNamespaceFailureStage::Observation => Self::Observation,
        }
    }
}

/// Fixed process-lifetime counts across all reserve namespace owners.
///
/// Each counter saturates. A concurrent snapshot samples fields independently and is not a
/// coherent multi-owner state, durable ledger, physical allocation measurement or SLO verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveTelemetrySnapshot {
    counts: [[u64; 8]; 3],
}
impl ReserveTelemetrySnapshot {
    /// Return the observed return count for one fixed attempt and decision pair.
    pub fn count(&self, attempt: ReserveAttempt, decision: ReserveDecision) -> u64 {
        self.counts[attempt as usize][decision as usize]
    }
}

struct ReserveCounters([[AtomicU64; 8]; 3]);
impl ReserveCounters {
    const fn new() -> Self {
        Self([const { [const { AtomicU64::new(0) }; 8] }; 3])
    }
    fn record(&self, attempt: ReserveAttempt, decision: ReserveDecision) {
        let _ = self.0[attempt as usize][decision as usize].fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| Some(count.saturating_add(1)),
        );
        tracing::info!(target: "yield::reserve_namespace",
            reserve_attempt = attempt.label(), reserve_decision = decision.label());
    }
    fn snapshot(&self) -> ReserveTelemetrySnapshot {
        ReserveTelemetrySnapshot {
            counts: std::array::from_fn(|attempt| {
                std::array::from_fn(|decision| self.0[attempt][decision].load(Ordering::Relaxed))
            }),
        }
    }
}
static COUNTERS: ReserveCounters = ReserveCounters::new();

/// Observe the fixed aggregate counters without reading reserve files or database rows.
pub fn reserve_telemetry_snapshot() -> ReserveTelemetrySnapshot {
    COUNTERS.snapshot()
}

pub(super) fn observe<T>(
    attempt: ReserveAttempt,
    result: Result<T, ReserveNamespaceFailure>,
) -> Result<T, ReserveNamespaceFailure> {
    let decision = result
        .as_ref()
        .err()
        .map_or(ReserveDecision::Bound, |failure| {
            ReserveDecision::from_failure(failure.stage())
        });
    COUNTERS.record(attempt, decision);
    result
}

#[cfg(test)]
pub(super) mod tests;
