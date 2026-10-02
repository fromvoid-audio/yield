//! Actual retained-owner publication outcomes, not simulated successful effects.
use super::*;
use crate::sample_sources::SourceFeaturePublicationMetrics;

fn assert_partition(metrics: SourceFeaturePublicationMetrics) {
    assert_eq!(
        metrics.attempts,
        metrics.recorded + metrics.duplicates + metrics.stale + metrics.cancelled + metrics.failed
    );
    assert!(metrics.total_elapsed_micros >= metrics.max_elapsed_micros);
    let json = serde_json::to_vec(&metrics).unwrap();
    assert!(json.len() <= 512);
    assert_eq!(
        serde_json::from_slice::<SourceFeaturePublicationMetrics>(&json).unwrap(),
        metrics
    );
}

#[test]
fn feature_metrics_real_commit_duplicate_conflict_and_cancellation_partition() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    assert_eq!(writer.feature_publication_metrics(), Default::default());
    let vector = payload(0.25);
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        writer.publish_feature_artifact(&revision, &claim, &vector, 11, &cancel),
        Ok(FeatureArtifactPublication::Recorded(_))
    ));
    assert!(matches!(
        writer.publish_feature_artifact(&revision, &claim, &vector, 12, &cancel),
        Ok(FeatureArtifactPublication::Duplicate(_))
    ));
    assert!(
        writer
            .publish_feature_artifact(&revision, &claim, &payload(0.5), 13, &cancel)
            .is_err()
    );
    assert!(matches!(
        writer.publish_feature_artifact(&revision, &claim, &vector, 14, &AtomicBool::new(true)),
        Err(SourceWriterError::Readiness(ReadinessError::Cancelled))
    ));
    let metrics = writer.feature_publication_metrics();
    assert_eq!(
        (
            metrics.attempts,
            metrics.recorded,
            metrics.duplicates,
            metrics.failed,
            metrics.cancelled,
            metrics.stale
        ),
        (4, 1, 1, 1, 1, 0)
    );
    assert_eq!(metrics.recorded_payload_bytes, 732);
    assert_eq!(count(&writer), 1);
    assert_partition(metrics);
    // Reading or serializing telemetry performs no writes and does not reset counters.
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert_eq!(writer.feature_publication_metrics(), metrics);
}

#[test]
fn feature_metrics_failed_transaction_and_expired_claim_record_no_bytes() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 5)
        .unwrap()
        .unwrap();
    writer.catalog_database().connection.execute_batch("CREATE TRIGGER fail_metric_publish BEFORE INSERT ON source_feature_artifacts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &payload(0.25),
                11,
                &AtomicBool::new(false)
            )
            .is_err()
    );
    writer
        .catalog_database()
        .connection
        .execute_batch("DROP TRIGGER fail_metric_publish")
        .unwrap();
    assert_eq!(
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &payload(0.25),
                16,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::RejectedStale
    );
    let metrics = writer.feature_publication_metrics();
    assert_eq!(
        (
            metrics.attempts,
            metrics.failed,
            metrics.stale,
            metrics.recorded_payload_bytes
        ),
        (2, 1, 1, 0)
    );
    assert_eq!(count(&writer), 0);
    let state: String = writer.catalog_database().connection.query_row("SELECT status FROM analysis_jobs WHERE readiness_scope_id='file-one' AND readiness_stage='analysis_features'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "running");
    assert_partition(metrics);
}

#[test]
fn feature_metrics_stale_revision_and_reacquisition_are_observed_without_authority() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let next = (revision.revision() + 1).to_string();
    for key in [
        "revision",
        crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_REVISION,
    ] {
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key=?2",
                rusqlite::params![next, key],
            )
            .unwrap();
    }
    assert!(matches!(
        writer.publish_feature_artifact(
            &revision,
            &claim,
            &payload(0.25),
            11,
            &AtomicBool::new(false)
        ),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    assert_eq!(writer.feature_publication_metrics().stale, 1);
    let database = writer.into_catalog_database();
    let mut writer =
        SourceWriter::new(database, SourceId::from_string("feature-owner"), 8).unwrap();
    assert_eq!(writer.feature_publication_metrics(), Default::default());
    let current = writer.current_revision().unwrap();
    assert!(matches!(
        writer.publish_feature_artifact(
            &current,
            &claim,
            &payload(0.25),
            12,
            &AtomicBool::new(false)
        ),
        Err(SourceWriterError::StaleFence)
    ));
    let metrics = writer.feature_publication_metrics();
    assert_eq!(
        (
            metrics.attempts,
            metrics.stale,
            metrics.recorded_payload_bytes
        ),
        (1, 1, 0)
    );
    assert_eq!(count(&writer), 0);
    assert_partition(metrics);
}
