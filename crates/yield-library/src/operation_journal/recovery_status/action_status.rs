//! Backend-neutral categories derived only from classified persisted recovery observations.
use super::{JournalRetentionKind, RecoveryStatusEntry, RecoveryStatusOutcome};
use crate::operation_journal::{FullProgressDisposition as F, OperationDisposition as L};

/// Infrastructure reason recorded for a retry, distinct from permission to retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalRetryBlockage {
    /// Bounded writer/resource contention.
    Contention,
    /// A participant was temporarily unavailable.
    Availability,
    /// Capacity was unavailable; this status does not reserve or release bytes.
    Capacity,
}

/// Cancellation boundary observed by the owning journal family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalCancellationBoundary {
    /// No application publication was recorded.
    BeforePublication,
    /// Application publication was recorded.
    AfterPublication,
    /// An effect was observed without claiming application publication (full-progress only).
    AfterObservedEffect,
}

/// Stable semantic status for UI adapters. No field grants an action or execution authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalUserStatus {
    /// Valid observation with no disposition overlay, still requiring coordinator recovery.
    InProgress,
    /// Persisted retry need, with an optional classified infrastructure blockage.
    RetryPending {
        /// Some participant work remains incomplete.
        partial: bool,
        /// Missing retry evidence remains unknown, never an actionable user retry.
        blockage: Option<JournalRetryBlockage>,
    },
    /// Evidence needs audit; a retained row never becomes complete or retryable.
    AuditRequired {
        /// Retained proof-gap category, absent for an explicit observed audit disposition.
        retained: Option<JournalRetentionKind>,
    },
    /// Cancellation was requested; completion is not inferred.
    CancellationRequested(JournalCancellationBoundary),
    /// Successful persisted disposition, independent of actual UI/native qualification.
    Succeeded {
        /// Optional artifacts remain deferred; aggregate convergence is not complete.
        deferred_artifacts: bool,
    },
    /// Cancellation completed at the recorded boundary.
    Cancelled(JournalCancellationBoundary),
    /// The record reports rollback.
    RolledBack,
    /// The durable disposition explicitly requires a user decision, not infrastructure retry.
    BlockedByUser,
    /// Failure retained data; no new retry action is inferred.
    FailedPreservingData,
    /// Unresolved data-loss risk remains visible and needs review.
    FailedDataLossRisk,
}

impl RecoveryStatusEntry {
    /// Derive status from durable disposition, never callback order or transient activity.
    /// UI adapters own wording and actions; this status grants neither retry nor success proof.
    pub fn user_status(&self) -> JournalUserStatus {
        match self.outcome() {
            RecoveryStatusOutcome::Retained(reason) => JournalUserStatus::AuditRequired {
                retained: Some(reason),
            },
            RecoveryStatusOutcome::Legacy { disposition, .. } => {
                legacy(disposition, self.retry_blockage)
            }
            RecoveryStatusOutcome::FullProgress { disposition, .. } => {
                full(disposition, self.retry_blockage)
            }
        }
    }

    /// Whether this is a later observation of the same validated operation and family.
    /// The caller partitions by profile. Unknown keys, another operation/family, duplicate
    /// and older observations cannot overwrite a current per-operation status.
    pub fn is_newer_observation_than(&self, previous: &Self) -> bool {
        self.operation_id().is_some()
            && self.operation_id() == previous.operation_id()
            && self.family() == previous.family()
            && self.sequence() > previous.sequence()
    }
}

fn legacy(disposition: L, blockage: Option<JournalRetryBlockage>) -> JournalUserStatus {
    use JournalCancellationBoundary::{AfterPublication, BeforePublication};
    use JournalUserStatus as S;
    match disposition {
        L::None => S::InProgress,
        L::RetryPending => S::RetryPending {
            partial: false,
            blockage,
        },
        L::PartialNeedsRetry => S::RetryPending {
            partial: true,
            blockage,
        },
        L::AuditRequired => S::AuditRequired { retained: None },
        L::CancelRequestedBeforePublish => S::CancellationRequested(BeforePublication),
        L::CancelRequestedAfterPublish => S::CancellationRequested(AfterPublication),
        L::Succeeded => S::Succeeded {
            deferred_artifacts: false,
        },
        L::SucceededWithDeferredArtifacts => S::Succeeded {
            deferred_artifacts: true,
        },
        L::CancelledBeforePublish => S::Cancelled(BeforePublication),
        L::CancelledAfterPublish => S::Cancelled(AfterPublication),
        L::RolledBack => S::RolledBack,
        L::BlockedByUser => S::BlockedByUser,
        L::FailedPreservingData => S::FailedPreservingData,
        L::FailedDataLossRisk => S::FailedDataLossRisk,
    }
}
fn full(disposition: F, blockage: Option<JournalRetryBlockage>) -> JournalUserStatus {
    use JournalCancellationBoundary::{AfterObservedEffect, AfterPublication, BeforePublication};
    use JournalUserStatus as S;
    match disposition {
        F::None => S::InProgress,
        F::RetryPending => S::RetryPending {
            partial: false,
            blockage,
        },
        F::PartialNeedsRetry => S::RetryPending {
            partial: true,
            blockage,
        },
        F::AuditRequired => S::AuditRequired { retained: None },
        F::CancelRequestedBeforePublish => S::CancellationRequested(BeforePublication),
        F::CancelRequestedAfterPublish => S::CancellationRequested(AfterPublication),
        F::CancelRequestedAfterObservedEffect => S::CancellationRequested(AfterObservedEffect),
        F::Succeeded => S::Succeeded {
            deferred_artifacts: false,
        },
        F::SucceededWithDeferredArtifacts => S::Succeeded {
            deferred_artifacts: true,
        },
        F::CancelledBeforePublish => S::Cancelled(BeforePublication),
        F::CancelledAfterPublish => S::Cancelled(AfterPublication),
        F::CancelledAfterObservedEffect => S::Cancelled(AfterObservedEffect),
        F::RolledBack => S::RolledBack,
        F::BlockedByUser => S::BlockedByUser,
        F::FailedPreservingData => S::FailedPreservingData,
        F::FailedDataLossRisk => S::FailedDataLossRisk,
    }
}
