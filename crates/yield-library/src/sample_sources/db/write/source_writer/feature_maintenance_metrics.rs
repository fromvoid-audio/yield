//! Fixed-size observations of real owner maintenance; no physical reclamation authority.
use super::{FeaturePrunePage, SourceWriterError};
use crate::sample_sources::readiness::ReadinessError;
use serde::Serialize;
use std::time::Duration;

/// Process-local totals for this writer acquisition. Logical SQL payload bytes do not
/// measure SQLite allocation, external cache cleanup or source retirement. If saturated,
/// affected totals are lower bounds. No source, path, key, error or history labels exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FeatureMaintenanceMetrics {
    /// Every actual call, including rejection before SQL.
    pub attempted: u64,
    /// Successful committed reports, including unchanged or ambiguous retention.
    pub committed: u64,
    /// Rejected current owner/revision or private cursor.
    pub stale: u64,
    /// Cancellation prevented commit.
    pub cancelled: u64,
    /// Other rejection or failure, including WAL rejection and rolled-back deletion.
    pub failed: u64,
    /// Candidates inspected in successful committed reports only.
    pub inspected_candidates: u64,
    /// Payload rows removed by successful committed transactions only.
    pub removed_rows: u64,
    /// Logical payload bytes removed; never physically reclaimed allocation.
    pub logical_payload_bytes_removed: u64,
    /// Sum of monotonic call elapsed microseconds; not a benchmark or SLO claim.
    pub total_elapsed_us: u64,
    /// Largest monotonic call elapsed observation.
    pub max_elapsed_us: u64,
    /// A counter or elapsed observation exceeded its scalar representation.
    pub saturated: bool,
}

fn add(slot: &mut u64, amount: u64, saturated: &mut bool) {
    match slot.checked_add(amount) {
        Some(value) => *slot = value,
        None => {
            *slot = u64::MAX;
            *saturated = true;
        }
    }
}
impl FeatureMaintenanceMetrics {
    pub(super) fn record(
        &mut self,
        result: &Result<FeaturePrunePage, SourceWriterError>,
        elapsed: Duration,
    ) {
        macro_rules! count {
            ($field:ident, $amount:expr) => {
                add(&mut self.$field, $amount, &mut self.saturated)
            };
        }
        count!(attempted, 1);
        let elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or_else(|_| {
            self.saturated = true;
            u64::MAX
        });
        count!(total_elapsed_us, elapsed_us);
        self.max_elapsed_us = self.max_elapsed_us.max(elapsed_us);
        match result {
            Ok(page) => {
                count!(committed, 1);
                count!(inspected_candidates, u64::from(page.observed));
                count!(removed_rows, u64::from(page.removed));
                count!(
                    logical_payload_bytes_removed,
                    page.logical_payload_bytes_removed
                );
            }
            Err(SourceWriterError::StaleFence | SourceWriterError::StaleRevision { .. }) => {
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
    fn maintenance_scalar_saturation_and_serialization_are_bounded() {
        // Representation only: actual owner/SQL outcomes are tested in curation fixtures.
        let mut metrics = FeatureMaintenanceMetrics {
            attempted: u64::MAX,
            committed: u64::MAX,
            inspected_candidates: u64::MAX,
            removed_rows: u64::MAX,
            logical_payload_bytes_removed: u64::MAX,
            total_elapsed_us: u64::MAX,
            ..Default::default()
        };
        metrics.record(
            &Ok(FeaturePrunePage {
                observed: 2,
                removed: 1,
                logical_payload_bytes_removed: 732,
                next: None,
            }),
            Duration::from_micros(4),
        );
        assert!(metrics.saturated);
        assert_eq!(metrics.attempted, u64::MAX);
        assert_eq!(metrics.committed, u64::MAX);
        assert_eq!(metrics.inspected_candidates, u64::MAX);
        assert_eq!(metrics.removed_rows, u64::MAX);
        assert_eq!(metrics.logical_payload_bytes_removed, u64::MAX);
        assert_eq!(metrics.total_elapsed_us, u64::MAX);
        assert_eq!(metrics.max_elapsed_us, 4);
        metrics.record(
            &Err(SourceWriterError::StaleFence),
            Duration::from_secs(u64::MAX),
        );
        assert_eq!(metrics.max_elapsed_us, u64::MAX);
        assert_eq!(metrics.stale, 1);
        let encoded = serde_json::to_string(&metrics).unwrap();
        assert!(encoded.len() < 640);
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
