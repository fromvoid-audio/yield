//! Fixed-size diagnostics for actual retained-owner rebuild admission, not physical cleanup.
use super::SourceWriterError;
use crate::sample_sources::readiness::{FeatureArtifactRebuildAdmission, ReadinessError};
use serde::Serialize;
use std::time::Duration;

/// Process-local observations of this retained writer's exact feature rebuild commands.
/// Counters reset on reacquisition. Elapsed time includes validation/SQL, not DSP. Logical
/// payload removal does not measure physical disk reclamation or retirement. No authority is
/// represented. With `saturated`, affected numeric values are lower bounds, not exact totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FeatureRebuildMetrics {
    /// Number of rebuild command calls, including rejection before SQL.
    pub attempted: u64,
    /// Existing healthy payload and matching completion required no repair.
    pub ready: u64,
    /// Exact work/marker repair transaction committed.
    pub admitted: u64,
    /// Ordinary pending/failed work retained its existing policy.
    pub unchanged: u64,
    /// An unexpired live claim deferred repair.
    pub deferred: u64,
    /// Ambiguous identity or control state retained evidence.
    pub ambiguous: u64,
    /// Current owner/revision/target rejection, without successful repair.
    pub stale: u64,
    /// Cancellation prevented repair commit.
    pub cancelled: u64,
    /// Other errors, including rolled-back cleanup, without successful repair.
    pub failed: u64,
    /// Demonstrably corrupt current-key payloads actually removed by committed repair.
    pub corrupt_payloads_removed: u64,
    /// Logical payload bytes removed from SQL rows; never physical disk bytes reclaimed.
    pub logical_payload_bytes_removed: u64,
    /// Sum of observed elapsed microseconds; not a benchmark or durability claim.
    pub total_elapsed_us: u64,
    /// Largest observed elapsed microseconds.
    pub max_elapsed_us: u64,
    /// At least one counter or elapsed observation exceeded its scalar representation.
    pub saturated: bool,
}
fn add(slot: &mut u64, increment: u64, saturated: &mut bool) {
    match slot.checked_add(increment) {
        Some(value) => *slot = value,
        None => {
            *slot = u64::MAX;
            *saturated = true;
        }
    }
}
impl FeatureRebuildMetrics {
    pub(super) fn record(
        &mut self,
        result: &Result<FeatureArtifactRebuildAdmission, SourceWriterError>,
        elapsed: Duration,
    ) {
        macro_rules! count {
            ($field:ident,$amount:expr) => {
                add(&mut self.$field, $amount, &mut self.saturated)
            };
        }
        count!(attempted, 1);
        let elapsed_us = match u64::try_from(elapsed.as_micros()) {
            Ok(value) => value,
            Err(_) => {
                self.saturated = true;
                u64::MAX
            }
        };
        count!(total_elapsed_us, elapsed_us);
        self.max_elapsed_us = self.max_elapsed_us.max(elapsed_us);
        match result {
            Ok(FeatureArtifactRebuildAdmission::Ready) => count!(ready, 1),
            Ok(FeatureArtifactRebuildAdmission::Admitted {
                removed_corrupt_payload,
            }) => {
                count!(admitted, 1);
                if *removed_corrupt_payload {
                    count!(corrupt_payloads_removed, 1);
                    count!(logical_payload_bytes_removed, 732);
                }
            }
            Ok(FeatureArtifactRebuildAdmission::Unchanged) => count!(unchanged, 1),
            Ok(FeatureArtifactRebuildAdmission::Deferred) => count!(deferred, 1),
            Ok(FeatureArtifactRebuildAdmission::Ambiguous) => count!(ambiguous, 1),
            Ok(FeatureArtifactRebuildAdmission::RejectedStale)
            | Err(SourceWriterError::StaleFence | SourceWriterError::StaleRevision { .. }) => {
                count!(stale, 1)
            }
            Err(SourceWriterError::Readiness(ReadinessError::Cancelled)) => count!(cancelled, 1),
            Err(_) => count!(failed, 1),
        }
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;
    #[test]
    fn rebuild_metric_scalar_saturation_and_serialization_remain_bounded() {
        // Observer representation only. Real owner/SQL outcome tests live in curation fixtures.
        let mut metrics = FeatureRebuildMetrics {
            attempted: u64::MAX,
            admitted: u64::MAX,
            corrupt_payloads_removed: u64::MAX,
            logical_payload_bytes_removed: u64::MAX,
            total_elapsed_us: u64::MAX,
            ..Default::default()
        };
        metrics.record(
            &Ok(FeatureArtifactRebuildAdmission::Admitted {
                removed_corrupt_payload: true,
            }),
            Duration::from_micros(4),
        );
        assert!(metrics.saturated);
        assert_eq!(metrics.attempted, u64::MAX);
        assert_eq!(metrics.logical_payload_bytes_removed, u64::MAX);
        assert_eq!(metrics.max_elapsed_us, 4);
        let encoded = serde_json::to_string(&metrics).unwrap();
        assert!(encoded.len() < 768);
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert!(
            value
                .as_object()
                .unwrap()
                .values()
                .all(|v| v.is_u64() || v.is_boolean())
        );
    }
}
