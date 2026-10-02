//! Fixed-size diagnostics for the actual retained-writer publication command.
use super::SourceWriterError;
use crate::sample_sources::readiness::{FeatureArtifactPublication as Publication, ReadinessError};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Acquisition-local publication diagnostics. All counters saturate; no identity labels or history
/// are retained. Serialized values are observations, never ownership or durability authority.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceFeaturePublicationMetrics {
    /// Completed command attempts, including rejected admission and failed transactions.
    pub attempts: u64,
    /// Commands that committed payload/readiness association.
    pub recorded: u64,
    /// Exact retries returning the existing publication.
    pub duplicates: u64,
    /// Stale claim, target, writer instance, lifecycle or revision rejections.
    pub stale: u64,
    /// Commands returning the typed cancellation outcome.
    pub cancelled: u64,
    /// Other errors, including immutable conflict, capacity and SQL rollback.
    pub failed: u64,
    /// Payload bytes acknowledged by Recorded outcomes, excluding duplicates and failures.
    /// A re-associated existing payload also counts; this is not newly allocated storage.
    pub recorded_payload_bytes: u64,
    /// Total monotonic command duration in microseconds, including validation/commit.
    /// This excludes queue wait and DSP time; it is not a benchmark or SLO guarantee.
    pub total_elapsed_micros: u64,
    /// Longest observed command duration, with the same scope as total_elapsed_micros.
    pub max_elapsed_micros: u64,
}

impl SourceFeaturePublicationMetrics {
    /// Record exactly one returned outcome after the production command finishes.
    pub(super) fn record(
        &mut self,
        result: &Result<Publication, SourceWriterError>,
        payload_bytes: usize,
        elapsed: Duration,
    ) {
        self.attempts = self.attempts.saturating_add(1);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.total_elapsed_micros = self.total_elapsed_micros.saturating_add(micros);
        self.max_elapsed_micros = self.max_elapsed_micros.max(micros);
        let counter = match result {
            Ok(Publication::Recorded(_)) => {
                self.recorded_payload_bytes = self
                    .recorded_payload_bytes
                    .saturating_add(u64::try_from(payload_bytes).unwrap_or(u64::MAX));
                &mut self.recorded
            }
            Ok(Publication::Duplicate(_)) => &mut self.duplicates,
            Ok(Publication::RejectedStale)
            | Err(SourceWriterError::StaleFence | SourceWriterError::StaleRevision { .. }) => {
                &mut self.stale
            }
            Err(SourceWriterError::Readiness(ReadinessError::Cancelled)) => &mut self.cancelled,
            Err(_) => &mut self.failed,
        };
        *counter = counter.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_metrics_saturate_counts_and_elapsed_without_history() {
        let mut metrics = SourceFeaturePublicationMetrics {
            attempts: u64::MAX,
            stale: u64::MAX,
            total_elapsed_micros: u64::MAX - 1,
            ..Default::default()
        };
        metrics.record(&Ok(Publication::RejectedStale), 732, Duration::MAX);
        assert_eq!(metrics.attempts, u64::MAX);
        assert_eq!(metrics.stale, u64::MAX);
        assert_eq!(metrics.total_elapsed_micros, u64::MAX);
        assert_eq!(metrics.max_elapsed_micros, u64::MAX);
        assert_eq!(metrics.recorded_payload_bytes, 0);
        assert_eq!(std::mem::size_of_val(&metrics), 9 * 8);
    }
}
