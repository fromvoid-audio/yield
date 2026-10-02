use super::*;
use crate::{CatalogCommand, CatalogService, FeatureAnalysisCancellation, SourceId};
use yield_library::app_dirs::{AppRootGuard, WritableProfileGuard};

#[test]
fn real_paused_owner_reports_admission_dispatch_and_preserves_credits() {
    let root = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(root.path().to_path_buf()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let (service, gate) = CatalogService::start_with_test_gate(&guard).unwrap();
    let pause = gate.pause_next();
    let audit = || CatalogCommand::RequestAudit {
        source_id: SourceId::from_string("unregistered-private-source"),
        lifecycle_generation: 1,
    };
    let invalid = CatalogCommand::RequestFeatureAnalysis {
        source_id: SourceId::from_string("unregistered-private-source"),
        lifecycle_generation: 1,
        expected_revision: 1,
        request_id: 1,
        relative_path: "/private/sample-name.wav".into(),
        cancellation: FeatureAnalysisCancellation::new(),
    };
    assert_eq!(
        service.submit(invalid),
        Err(CatalogSubmitError::InvalidFeatureRequest)
    );
    for _ in 0..crate::COMMAND_CAPACITY {
        service.submit(audit()).unwrap();
    }
    assert_eq!(service.submit(audit()), Err(CatalogSubmitError::Full));
    let pending = service.try_command_metrics().unwrap();
    assert_eq!(pending.accepted, 32);
    assert_eq!(pending.invalid, 1);
    assert_eq!(pending.full, 1);
    assert_eq!(pending.pending_dispatch, 32);
    assert_eq!(pending.pending_high_water, 32);
    assert_eq!(
        pending.dispatched, 0,
        "accepted is not processed or committed"
    );
    assert_eq!(pending.admission_gate_wait.count, 34);
    assert!(!format!("{pending:?}").contains("private"));
    let held_admission = service.admission.lock().unwrap();
    assert_eq!(
        service.try_command_metrics(),
        None,
        "diagnostic must not wait for a held gate"
    );
    drop(held_admission);
    drop(pause);
    service.shutdown();
    assert_eq!(service.submit(audit()), Err(CatalogSubmitError::Closed));
    let drained = service.try_command_metrics().unwrap();
    assert_eq!(drained.closed, 1);
    assert_eq!(drained.dispatched, 32);
    assert_eq!(drained.pending_dispatch, 0);
    assert_eq!(drained.untracked_dispatches, 0);
    assert_eq!(drained.bookkeeping_overflow, 0);
    assert_eq!(drained.dispatch_wait.count, 32);
    assert_eq!(drained.dispatch_wait.buckets.iter().sum::<u64>(), 32);
    assert_eq!(drained.admission_gate_wait.count, 35);
    let admission = service.admission.lock().unwrap();
    assert_eq!(
        admission.event_budget, 1,
        "existing closed-event credit is retained until delivery"
    );
    assert_eq!(admission.curation_payload_bytes, 0);
    assert_eq!(admission.tag_page_input_bytes, 0);
    assert_eq!(admission.tag_read_payload_bytes, 0);
    drop(admission);
    assert!(matches!(
        service.try_recv_event(),
        Ok(crate::CatalogEvent::Closed)
    ));
    assert_eq!(service.admission.lock().unwrap().event_budget, 0);
}

#[test]
fn fixed_duration_buckets_and_counters_saturate() {
    let mut durations = CatalogDurationMetrics::default();
    for micros in [0, 99, 100, 999, 1_000, 9_999, 10_000, 99_999, 100_000] {
        durations.record(Duration::from_micros(micros));
    }
    assert_eq!(durations.buckets, [2, 2, 2, 2, 1]);
    durations.count = u64::MAX;
    durations.total_micros = u64::MAX;
    durations.buckets[4] = u64::MAX;
    durations.record(Duration::MAX);
    assert_eq!(durations.count, u64::MAX);
    assert_eq!(durations.total_micros, u64::MAX);
    assert_eq!(durations.max_micros, u64::MAX);
    assert_eq!(durations.buckets[4], u64::MAX);
    let mut metrics = CommandMetrics::new();
    metrics.snapshot.accepted = u64::MAX;
    metrics.submission(&Ok(()), Duration::ZERO);
    assert_eq!(metrics.snapshot().accepted, u64::MAX);
}

#[test]
fn timestamp_ring_is_bounded_and_wraps_in_fifo_order() {
    let mut metrics = CommandMetrics::new();
    let start = Instant::now();
    for round in 0..3 {
        for index in 0..PENDING_CAPACITY {
            metrics.accepted(start + Duration::from_micros(index as u64));
        }
        assert_eq!(metrics.snapshot().pending_dispatch, PENDING_CAPACITY as u32);
        for index in 0..PENDING_CAPACITY {
            metrics.received(start + Duration::from_micros(index as u64 + 500));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.pending_dispatch, 0);
        assert_eq!(snapshot.dispatched, (round + 1) * PENDING_CAPACITY as u64);
        assert_eq!(snapshot.dispatch_wait.max_micros, 500);
        assert_eq!(
            snapshot.dispatch_wait.total_micros,
            snapshot.dispatched * 500
        );
    }
    metrics.received(start);
    assert_eq!(metrics.snapshot().untracked_dispatches, 1);
    assert_eq!(
        metrics.snapshot().dispatch_wait.count,
        3 * PENDING_CAPACITY as u64
    );
}

#[test]
fn bookkeeping_loss_stops_timing_instead_of_correlating_unproven_receipts() {
    let mut metrics = CommandMetrics::new();
    let start = Instant::now();
    for _ in 0..=PENDING_CAPACITY {
        metrics.accepted(start);
    }
    assert_eq!(metrics.snapshot().bookkeeping_overflow, 1);
    assert!(!metrics.snapshot().dispatch_timing_available);
    metrics.accepted(start);
    metrics.received(start + Duration::from_secs(1));
    assert_eq!(metrics.snapshot().dispatch_wait.count, 0);
    assert_eq!(metrics.snapshot().untracked_dispatches, 1);
    assert_eq!(metrics.snapshot().pending_dispatch, 0);
}

#[test]
fn disconnected_channel_rejects_without_timestamp_or_credit_leak() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(1);
    drop(receiver);
    let service = CatalogService {
        commands,
        events: std::sync::Arc::new(crate::EventHub::new()),
        admission: std::sync::Arc::new(std::sync::Mutex::new(crate::AdmissionState::new())),
        worker: std::sync::Mutex::new(None),
    };
    assert_eq!(
        service.submit(CatalogCommand::RequestAudit {
            source_id: SourceId::from_string("private-source"),
            lifecycle_generation: 1,
        }),
        Err(CatalogSubmitError::Closed)
    );
    let snapshot = service.try_command_metrics().unwrap();
    assert_eq!(snapshot.closed, 1);
    assert_eq!(snapshot.accepted, 0);
    assert_eq!(snapshot.pending_dispatch, 0);
    assert_eq!(snapshot.dispatch_wait.count, 0);
    assert_eq!(service.admission.lock().unwrap().event_budget, 1);
}
