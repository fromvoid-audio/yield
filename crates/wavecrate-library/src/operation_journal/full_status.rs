//! V16-only progress dispositions for the guarded full-progress model.
//!
//! The legacy [`super::model::OperationDisposition`] remains the wire and status type for
//! schemas V1 through V15.  This parallel type adds cancellation states that describe an
//! observed effect without claiming ordinary application publication, preserving the legacy
//! vocabulary and its codecs.

use serde::{Deserialize, Serialize};

/// Guarded disposition overlay used by the full V16 progress model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FullProgressDisposition {
    /// No disposition overlay is currently active.
    None,
    /// Work may be retried by a future coordinator.
    RetryPending,
    /// A participant is incomplete and needs retry.
    PartialNeedsRetry,
    /// Evidence is uncertain and requires an audit.
    AuditRequired,
    /// Cancellation was requested before publication.
    CancelRequestedBeforePublish,
    /// Cancellation was requested after ordinary publication.
    CancelRequestedAfterPublish,
    /// Cancellation was requested after an observed effect without application publication.
    CancelRequestedAfterObservedEffect,
    /// Operation completed successfully.
    Succeeded,
    /// Operation completed while optional artifacts remain deferred.
    SucceededWithDeferredArtifacts,
    /// Operation was cancelled before publication.
    CancelledBeforePublish,
    /// Operation was cancelled after ordinary publication.
    CancelledAfterPublish,
    /// Operation was cancelled after an observed effect without application publication.
    CancelledAfterObservedEffect,
    /// Operation was rolled back.
    RolledBack,
    /// Operation is blocked pending a user decision.
    BlockedByUser,
    /// Operation failed while preserving evidence.
    FailedPreservingData,
    /// Operation has unresolved data-loss risk.
    FailedDataLossRisk,
}

impl FullProgressDisposition {
    /// Return whether this disposition terminates the operation.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::SucceededWithDeferredArtifacts
                | Self::CancelledBeforePublish
                | Self::CancelledAfterPublish
                | Self::CancelledAfterObservedEffect
                | Self::RolledBack
                | Self::BlockedByUser
                | Self::FailedPreservingData
                | Self::FailedDataLossRisk
        )
    }
}

#[cfg(test)]
mod tests {
    use super::FullProgressDisposition;
    use crate::operation_journal::OperationDisposition;

    #[test]
    fn observed_effect_variants_are_full_status_only() {
        for (json, expected) in [
            (
                r#""CancelRequestedAfterObservedEffect""#,
                FullProgressDisposition::CancelRequestedAfterObservedEffect,
            ),
            (
                r#""CancelledAfterObservedEffect""#,
                FullProgressDisposition::CancelledAfterObservedEffect,
            ),
        ] {
            let decoded: FullProgressDisposition =
                serde_json::from_str(json).expect("full status variant decodes");
            assert_eq!(decoded, expected);
            assert!(serde_json::from_str::<OperationDisposition>(json).is_err());
        }
    }

    #[test]
    fn full_status_preserves_terminal_classification() {
        assert!(!FullProgressDisposition::RetryPending.is_terminal());
        assert!(!FullProgressDisposition::CancelRequestedAfterObservedEffect.is_terminal());
        assert!(FullProgressDisposition::CancelledAfterObservedEffect.is_terminal());
        assert!(FullProgressDisposition::Succeeded.is_terminal());
    }
}
