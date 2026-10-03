//! Bounded real normal-build catalog/scanner/WAV-DSP/SQLite observations, not qualification.
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};
use yield_catalog::{
    CatalogCommand, CatalogEvent, CatalogService, FeatureAnalysisBlocker,
    FeatureAnalysisCancellation, FeatureAnalysisMetrics, FeatureAnalysisState,
};
use yield_library::{
    app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard},
    sample_sources::{SourceDatabase, SourceDatabaseConnectionRole, SourceId},
};

const EVENT_LIMIT: usize = 64;
const PREFIX: &str = "YIELD_CATALOG_ANALYSIS_EVIDENCE=";

fn deadline(total: Instant) -> Instant {
    total.min(Instant::now() + Duration::from_secs(5))
}
fn event(
    service: &CatalogService,
    total: Instant,
    predicate: impl Fn(&CatalogEvent) -> bool,
) -> CatalogEvent {
    let end = deadline(total);
    for _ in 0..EVENT_LIMIT {
        let e = service
            .recv_event_timeout(end.saturating_duration_since(Instant::now()))
            .expect("bounded actual owner event");
        if predicate(&e) {
            return e;
        }
    }
    panic!("owner event count exceeded");
}
fn register(service: &CatalogService, source: &SourceId, root: &Path, total: Instant) -> u64 {
    service
        .submit(CatalogCommand::Register {
            source_id: source.clone(),
            canonical_root: root.into(),
            lifecycle_generation: 1,
        })
        .unwrap();
    event(
        service,
        total,
        |e| matches!(e, CatalogEvent::Registered { source_id, .. } if source_id == source),
    );
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: source.clone(),
            lifecycle_generation: 1,
        })
        .unwrap();
    match event(
        service,
        total,
        |e| matches!(e, CatalogEvent::AuditCommitted { snapshot } if &snapshot.source_id == source),
    ) {
        CatalogEvent::AuditCommitted { snapshot } => {
            assert_eq!(snapshot.manifest.len(), 1);
            snapshot.committed_revision
        }
        _ => unreachable!(),
    }
}
fn metrics(m: FeatureAnalysisMetrics) -> Value {
    json!({"entered": m.entered, "dropped_before_work": m.dropped_before_work,
        "terminal_ready": m.terminal_ready, "terminal_blocked": m.terminal_blocked,
        "terminal_stale": m.terminal_stale, "terminal_cancelled": m.terminal_cancelled,
        "terminal_enqueued": m.terminal_enqueued, "terminal_projection_rejected": m.terminal_projection_rejected,
        "total_elapsed_us": m.total_elapsed_us, "max_elapsed_us": m.max_elapsed_us, "saturated": m.saturated})
}
fn settled(service: &CatalogService, entered: u64, total: Instant) -> FeatureAnalysisMetrics {
    let end = deadline(total);
    loop {
        if let Some(m) = service.try_feature_analysis_metrics() {
            assert!(!m.saturated);
            if m.entered == entered
                && m.dropped_before_work + m.terminal_ready + m.terminal_blocked == entered
            {
                assert_eq!(
                    m.terminal_enqueued + m.terminal_projection_rejected,
                    m.terminal_ready + m.terminal_blocked
                );
                assert!(m.total_elapsed_us >= m.max_elapsed_us);
                return m;
            }
            assert!(m.entered <= entered, "unexpected analysis request");
        }
        assert!(Instant::now() < end, "worker metric settlement timeout");
        thread::sleep(Duration::from_millis(1));
    }
}
fn sql(guard: &WritableProfileGuard, source: &SourceId, root: &Path) -> Value {
    // Match the existing catalog-owned profile child layout; this fixture opens only UiRead.
    let child = format!(
        "yield-catalog-{}",
        blake3::hash(source.as_str().as_bytes()).to_hex()
    );
    let connection = SourceDatabase::open_connection_with_role_and_database_root(
        root,
        guard.profile_root().join(child),
        SourceDatabaseConnectionRole::UiRead,
    )
    .unwrap();
    let (payload_rows, payload_bytes, marker_rows, failed_jobs, retryable_jobs): (u64,u64,u64,u64,u64) = connection.query_row(
        "SELECT (SELECT count(*) FROM source_feature_artifacts),
        (SELECT coalesce(sum(length(payload)),0) FROM source_feature_artifacts),
        (SELECT count(*) FROM source_readiness_artifacts WHERE stage='analysis_features'),
        (SELECT count(*) FROM analysis_jobs WHERE readiness_stage='analysis_features' AND status='failed'),
        (SELECT count(*) FROM analysis_jobs WHERE readiness_stage='analysis_features' AND failure_kind='retryable')", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert!(
        payload_rows <= 1
            && marker_rows <= 1
            && payload_bytes <= 8192
            && failed_jobs <= 1
            && retryable_jobs <= 1
    );
    json!({"payload_rows":payload_rows,"payload_bytes":payload_bytes,"marker_rows":marker_rows,"failed_jobs":failed_jobs,"retryable_jobs":retryable_jobs})
}
#[allow(clippy::too_many_arguments)]
fn case(
    service: &CatalogService,
    guard: &WritableProfileGuard,
    source: &SourceId,
    root: &Path,
    revision: u64,
    request: u64,
    name: &str,
    expected: &str,
    cancel: bool,
    total: Instant,
) -> (Value, FeatureAnalysisState) {
    let before = settled(service, request - 1, total);
    let sql_before = sql(guard, source, root);
    let start = Instant::now();
    let cancellation = FeatureAnalysisCancellation::new();
    if cancel {
        cancellation.cancel();
    }
    service
        .submit(CatalogCommand::RequestFeatureAnalysis {
            source_id: source.clone(),
            lifecycle_generation: 1,
            expected_revision: revision,
            request_id: request,
            relative_path: "tone.wav".into(),
            cancellation,
        })
        .unwrap();
    let end = deadline(total);
    let mut previous = None;
    let mut count = 0;
    let terminal = loop {
        count += 1;
        assert!(count <= EVENT_LIMIT);
        let e = service
            .recv_event_timeout(end.saturating_duration_since(Instant::now()))
            .expect("actual feature terminal timeout");
        if let CatalogEvent::FeatureAnalysisState {
            source_id,
            lifecycle_generation,
            request_id,
            state,
            ..
        } = e
        {
            assert_eq!(source_id, *source);
            assert_eq!(lifecycle_generation, 1);
            assert_eq!(request_id, request);
            assert!(state.follows(previous.as_ref()), "actual phase ordering");
            if matches!(
                state,
                FeatureAnalysisState::Ready { .. } | FeatureAnalysisState::Blocked { .. }
            ) {
                break state;
            }
            previous = Some(state);
        }
    };
    let result = match &terminal {
        FeatureAnalysisState::Ready { .. } => "ready",
        FeatureAnalysisState::Blocked {
            reason: FeatureAnalysisBlocker::Computation,
        } => "blocked_computation",
        FeatureAnalysisState::Blocked {
            reason: FeatureAnalysisBlocker::StaleSource,
        } => "blocked_stale",
        FeatureAnalysisState::Blocked {
            reason: FeatureAnalysisBlocker::Cancelled,
        } => "blocked_cancelled",
        _ => panic!("unexpected actual terminal classification"),
    };
    assert_eq!(result, expected);
    let after = settled(service, request, total);
    let sql_after = sql(guard, source, root);
    let wall_us = u64::try_from(start.elapsed().as_micros()).unwrap();
    assert!(wall_us <= 5_000_000 && Instant::now() < total);
    assert_eq!(after.entered - before.entered, 1);
    assert_eq!(after.terminal_enqueued - before.terminal_enqueued, 1);
    assert_eq!(
        after.terminal_projection_rejected,
        before.terminal_projection_rejected
    );
    assert!(after.total_elapsed_us - before.total_elapsed_us <= wall_us);
    (
        json!({"case":name,"result":result,"before":metrics(before),"after":metrics(after),
        "sql_before":sql_before,"sql_after":sql_after,"received_events":count,"wall_us":wall_us}),
        terminal,
    )
}

#[test]
#[ignore = "explicit bounded actual analysis evidence collector"]
fn actual_catalog_analysis_outcomes_and_metrics() {
    let total = Instant::now() + Duration::from_secs(40);
    let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
    let config = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
    let _profile = PersistenceProfileGuard::named("catalog-analysis-evidence");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let valid = tempfile::tempdir().unwrap();
    let invalid = tempfile::tempdir().unwrap();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(valid.path().join("tone.wav"), spec).unwrap();
    for i in 0..16_000 {
        writer
            .write_sample(
                ((i as f64 * 440.0 * std::f64::consts::TAU / 16_000.0).sin() * 12_000.0) as i16,
            )
            .unwrap();
    }
    writer.finalize().unwrap();
    std::fs::write(invalid.path().join("tone.wav"), b"invalid real WAV bytes").unwrap();
    let valid_source = SourceId::from_string("analysis-evidence-valid-v1");
    let invalid_source = SourceId::from_string("analysis-evidence-invalid-v1");
    let service = CatalogService::start(&guard).unwrap(); // Catalog dependency is compiled without cfg(test).
    let valid_revision = register(&service, &valid_source, valid.path(), total);
    let invalid_revision = register(&service, &invalid_source, invalid.path(), total);
    let mut samples = Vec::with_capacity(5);
    let (sample, first) = case(
        &service,
        &guard,
        &valid_source,
        valid.path(),
        valid_revision,
        1,
        "ready",
        "ready",
        false,
        total,
    );
    samples.push(sample);
    let (sample, retry) = case(
        &service,
        &guard,
        &valid_source,
        valid.path(),
        valid_revision,
        2,
        "already_ready",
        "ready",
        false,
        total,
    );
    samples.push(sample);
    assert_eq!(first, retry, "actual validated ready key reuse");
    let (sample, _) = case(
        &service,
        &guard,
        &invalid_source,
        invalid.path(),
        invalid_revision,
        3,
        "invalid_wav",
        "blocked_computation",
        false,
        total,
    );
    samples.push(sample);
    let (sample, _) = case(
        &service,
        &guard,
        &valid_source,
        valid.path(),
        valid_revision.checked_add(1).unwrap(),
        4,
        "stale_revision",
        "blocked_stale",
        false,
        total,
    );
    samples.push(sample);
    let (sample, _) = case(
        &service,
        &guard,
        &valid_source,
        valid.path(),
        valid_revision,
        5,
        "cancelled",
        "blocked_cancelled",
        true,
        total,
    );
    samples.push(sample);
    assert_eq!(
        sql(&guard, &valid_source, valid.path()),
        json!({"payload_rows":1,"payload_bytes":732,"marker_rows":1,"failed_jobs":0,"retryable_jobs":0})
    );
    assert_eq!(
        sql(&guard, &invalid_source, invalid.path()),
        json!({"payload_rows":0,"payload_bytes":0,"marker_rows":0,"failed_jobs":1,"retryable_jobs":1})
    );
    service.shutdown();
    let restarted = CatalogService::start(&guard).unwrap();
    let reset = settled(&restarted, 0, total);
    assert_eq!(reset, FeatureAnalysisMetrics::default());
    assert_eq!(sql(&guard, &valid_source, valid.path())["payload_rows"], 1);
    assert_eq!(
        sql(&guard, &invalid_source, invalid.path())["failed_jobs"],
        1
    );
    restarted.shutdown();
    let record = json!({"schema":"yield.catalog-analysis-case.v1","fixture":"actual-normal-build-catalog-v1",
        "files":2,"requests":5,"frame_count":16000,"sample_rate":16000,"samples":samples,
        "ready_key_reused":true,"reset":metrics(reset),"restart_sql_retained":true,
        "ui_acknowledged":false,"physical_qualification":false,"slo_claim":false});
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(encoded.len() <= 16 * 1024 && Instant::now() < total);
    println!("\n{PREFIX}{encoded}");
}
