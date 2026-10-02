//! Fixed-cardinality observations; never continuity or persistence authority.

use super::{AdmissionOutcome, AdmissionRejectReason};
use crate::sample_sources::SourceId;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bounded counters for one retained reconciliation supervisor.
///
/// Repeated observations are counted, not durable transitions. No source identity or raw
/// evidence is retained here. Histogram buckets are cumulative only within each bucket.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatcherMetrics {
    /// Continuity-proven admissions, before any durable checkpoint acknowledgement.
    pub admitted_continuity: u64,
    /// Accepted unproven observations that require authoritative audit.
    pub admitted_unproven: u64,
    /// Exact duplicate envelopes suppressed by the supervisor.
    pub duplicates: u64,
    /// Rejected source generation or physical-root mismatches.
    pub stale_admissions: u64,
    /// Other rejected admissions and retained-capacity exhaustion.
    pub rejected_admissions: u64,
    /// Validated committed replay checkpoints that retired a live Applied ticket.
    pub acknowledged_checkpoints: u64,
    /// Checkpoint calls that did not supply matching live committed authority.
    pub rejected_checkpoints: u64,
    /// Current unresolved retained uncertainty markers, including emergency markers.
    pub retained_proof_gaps: usize,
    /// Admission-to-acknowledgement counts in <=1/10/100/1000/10000/60000 ms and overflow.
    /// Only successful validated checkpoints contribute, using monotonic elapsed time.
    pub acknowledgement_lag_buckets: [u64; 7],
}

#[derive(Clone, Copy)]
pub(super) enum ContinuityOutcome {
    AdmittedContinuity,
    AdmittedUnproven,
    Duplicate,
    Stale,
    Rejected,
    Acknowledged,
    CheckpointRejected,
}

impl ContinuityOutcome {
    pub(super) fn admission(outcome: &AdmissionOutcome, continuity: bool) -> Self {
        match outcome {
            AdmissionOutcome::Accepted(_) if continuity => Self::AdmittedContinuity,
            AdmissionOutcome::Accepted(_) => Self::AdmittedUnproven,
            AdmissionOutcome::DuplicateSuppressed(_) => Self::Duplicate,
            AdmissionOutcome::Rejected(rejected)
                if matches!(
                    rejected,
                    AdmissionRejectReason::StaleGeneration | AdmissionRejectReason::WrongRoot
                ) =>
            {
                Self::Stale
            }
            AdmissionOutcome::Rejected(_) | AdmissionOutcome::UncertaintyCapacityExhausted(_) => {
                Self::Rejected
            }
        }
    }

    fn fields(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::AdmittedContinuity => ("admitted_continuity", "unverified", "checkpoint_pending"),
            Self::AdmittedUnproven => ("admitted_unproven", "missing", "audit_required"),
            Self::Duplicate => (
                "duplicate",
                "unverified",
                "duplicate_is_not_acknowledgement",
            ),
            Self::Stale => ("stale", "stale", "admission_identity_mismatch"),
            Self::Rejected => ("rejected", "unverified", "admission_rejected"),
            Self::Acknowledged => ("acknowledged", "none", "committed_checkpoint"),
            Self::CheckpointRejected => (
                "checkpoint_rejected",
                "unverified",
                "checkpoint_authority_rejected",
            ),
        }
    }
}

impl WatcherMetrics {
    pub(super) fn observe(
        &mut self,
        outcome: ContinuityOutcome,
        source: Option<&SourceId>,
        retained: usize,
        lag: Option<Duration>,
    ) {
        let counter = match outcome {
            ContinuityOutcome::AdmittedContinuity => &mut self.admitted_continuity,
            ContinuityOutcome::AdmittedUnproven => &mut self.admitted_unproven,
            ContinuityOutcome::Duplicate => &mut self.duplicates,
            ContinuityOutcome::Stale => &mut self.stale_admissions,
            ContinuityOutcome::Rejected => &mut self.rejected_admissions,
            ContinuityOutcome::Acknowledged => &mut self.acknowledged_checkpoints,
            ContinuityOutcome::CheckpointRejected => &mut self.rejected_checkpoints,
        };
        *counter = counter.saturating_add(1);
        self.retained_proof_gaps = retained;
        let lag_ms = lag.map(|lag| u64::try_from(lag.as_millis()).unwrap_or(u64::MAX));
        if let Some(ms) = lag_ms {
            let bucket = [1, 10, 100, 1_000, 10_000, 60_000]
                .iter()
                .position(|bound| ms <= *bound)
                .unwrap_or(6);
            self.acknowledgement_lag_buckets[bucket] =
                self.acknowledgement_lag_buckets[bucket].saturating_add(1);
        }
        let (continuity_outcome, proof_gap_kind, evidence_code) = outcome.fields();
        let observed_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
        tracing::info!(target: "yield::watcher_metrics", name = "watcher.continuity_observed",
            schema_version = 1_u64, observed_unix_ms, origin = "live",
            source_id = source.map(SourceId::as_str), continuity_outcome,
            proof_gap_kind, component = "retained_replay", evidence_code,
            retained_proof_gaps = retained, acknowledgement_lag_ms = lag_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::{Event, Metadata, Subscriber, span};

    type Fields = BTreeMap<String, String>;
    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<Fields>>>);
    impl Subscriber for Collector {
        fn register_callsite(
            &self,
            metadata: &'static Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            if metadata.target() == "yield::watcher_metrics" {
                tracing::subscriber::Interest::always()
            } else {
                tracing::subscriber::Interest::never()
            }
        }
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "yield::watcher_metrics"
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            struct Visitor(Fields);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.insert(field.name().into(), format!("{value:?}"));
                }
            }
            let mut visitor = Visitor(Fields::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    #[test]
    fn emitted_fields_are_bounded_and_lag_only_describes_acknowledgement() {
        // Other library fixtures install process-global tracing filters. Isolate the event
        // subscriber so their concurrent callsite-cache updates cannot disable this collector.
        const CHILD: &str = "YIELD_WATCHER_TELEMETRY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sample_sources::reconciliation::telemetry::tests::emitted_fields_are_bounded_and_lag_only_describes_acknowledgement", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "isolated emitter test: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let collector = Collector::default();
        let mut metrics = WatcherMetrics::default();
        tracing::subscriber::with_default(collector.clone(), || {
            for outcome in [
                ContinuityOutcome::AdmittedContinuity,
                ContinuityOutcome::AdmittedUnproven,
                ContinuityOutcome::Duplicate,
                ContinuityOutcome::Stale,
                ContinuityOutcome::Rejected,
                ContinuityOutcome::CheckpointRejected,
            ] {
                metrics.observe(outcome, None, 3, None);
            }
            for ms in [
                0, 1, 2, 10, 11, 100, 101, 1_000, 1_001, 10_000, 10_001, 60_000, 60_001,
            ] {
                metrics.observe(
                    ContinuityOutcome::Acknowledged,
                    Some(&SourceId::from_string("source-a")),
                    3,
                    Some(Duration::from_millis(ms)),
                );
            }
        });
        assert_eq!(metrics.acknowledgement_lag_buckets, [2, 2, 2, 2, 2, 2, 1]);
        let events = collector.0.lock().unwrap();
        assert_eq!(events.len(), 19);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["name"], "\"watcher.continuity_observed\"");
            assert_eq!(event["schema_version"], "1");
            assert_eq!(event["origin"], "\"live\"");
            assert!(event.contains_key("observed_unix_ms"));
            assert_eq!(event.contains_key("acknowledgement_lag_ms"), index >= 6);
            assert_eq!(event.contains_key("source_id"), index >= 6);
            for field in event.keys() {
                assert!(
                    [
                        "name",
                        "schema_version",
                        "origin",
                        "observed_unix_ms",
                        "source_id",
                        "continuity_outcome",
                        "proof_gap_kind",
                        "component",
                        "evidence_code",
                        "retained_proof_gaps",
                        "acknowledgement_lag_ms"
                    ]
                    .contains(&field.as_str()),
                    "unexpected {field}"
                );
            }
        }
    }

    #[test]
    fn counters_saturate_without_wrapping_or_growing_state() {
        let mut metrics = WatcherMetrics {
            duplicates: u64::MAX,
            ..WatcherMetrics::default()
        };
        metrics.observe(ContinuityOutcome::Duplicate, None, 1, None);
        assert_eq!(metrics.duplicates, u64::MAX);
        assert_eq!(metrics.retained_proof_gaps, 1);
    }
}
