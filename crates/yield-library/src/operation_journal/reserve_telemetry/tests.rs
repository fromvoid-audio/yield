use super::*;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

#[derive(Clone, Default)]
pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<Vec<(String, String)>>>>);
impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "yield::reserve_namespace"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Fields(Vec<(String, String)>);
        impl Visit for Fields {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.push((field.name().into(), value.into()));
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().into(), format!("{value:?}")));
            }
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn fixed_stage_mapping_counts_and_trace_fields_are_complete() {
    let counters = ReserveCounters::new();
    let stages = [
        ReserveNamespaceFailureStage::Unsupported,
        ReserveNamespaceFailureStage::RootValidation,
        ReserveNamespaceFailureStage::Open,
        ReserveNamespaceFailureStage::Allocation,
        ReserveNamespaceFailureStage::Sync,
        ReserveNamespaceFailureStage::Binding,
        ReserveNamespaceFailureStage::Observation,
    ];
    let capture = Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        for attempt in [
            ReserveAttempt::Create,
            ReserveAttempt::Reopen,
            ReserveAttempt::ReopenPartial,
        ] {
            counters.record(attempt, ReserveDecision::Bound);
            for stage in stages {
                counters.record(attempt, ReserveDecision::from_failure(stage));
            }
        }
    });
    assert_eq!(counters.snapshot().counts, [[1; 8]; 3]);
    let records = capture.0.lock().unwrap();
    assert_eq!(records.len(), 24);
    for fields in &*records {
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "reserve_attempt");
        assert!(["create", "reopen", "reopen_partial"].contains(&fields[0].1.as_str()));
        assert_eq!(fields[1].0, "reserve_decision");
        assert!(
            [
                "bound",
                "unsupported",
                "root_validation",
                "open",
                "allocation",
                "sync",
                "binding",
                "observation"
            ]
            .contains(&fields[1].1.as_str())
        );
    }
}

#[test]
fn counts_saturate_without_cross_category_or_owner_registry() {
    let counters = ReserveCounters::new();
    counters.0[0][1].store(u64::MAX, Ordering::Relaxed);
    counters.record(ReserveAttempt::Create, ReserveDecision::Unsupported);
    counters.record(ReserveAttempt::Reopen, ReserveDecision::Unsupported);
    let snapshot = counters.snapshot();
    assert_eq!(
        snapshot.count(ReserveAttempt::Create, ReserveDecision::Unsupported),
        u64::MAX
    );
    assert_eq!(
        snapshot.count(ReserveAttempt::Reopen, ReserveDecision::Unsupported),
        1
    );
    assert_eq!(
        snapshot.count(ReserveAttempt::Create, ReserveDecision::Bound),
        0
    );
    assert_eq!(
        std::mem::size_of::<ReserveCounters>(),
        24 * std::mem::size_of::<AtomicU64>()
    );
}
