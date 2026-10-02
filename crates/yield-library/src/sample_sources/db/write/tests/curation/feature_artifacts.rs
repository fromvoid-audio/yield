use super::*;
use crate::sample_sources::readiness::{
    FeatureArtifactPayload, FeatureArtifactPublication, ReadinessError,
};

pub(super) fn prepare() -> (CurationFixture, ReadinessTarget) {
    let mut fixture = CurationFixture::new_profile_bound();
    fixture
        .database
        .connection
        .execute(
            "UPDATE wav_files SET file_identity = CASE path WHEN 'one.wav' THEN 'file-one' ELSE 'file-two' END",
            [],
        )
        .unwrap();
    let target = ReadinessTarget::file(
        "feature-owner",
        "file-one",
        "one.wav",
        ReadinessStage::AnalysisFeatures,
        "features-v1",
        1,
        "content-one",
    );
    let mut targets = vec![target.clone()];
    for stage in [
        ReadinessStage::IndexedIdentity,
        ReadinessStage::EmbeddingAspects,
    ] {
        let mut other = target.clone();
        other.stage = stage;
        other.required_version = "fixture-other-v1".to_owned();
        targets.push(other);
    }
    for stage in [
        ReadinessStage::IndexedIdentity,
        ReadinessStage::AnalysisFeatures,
        ReadinessStage::EmbeddingAspects,
    ] {
        targets.push(ReadinessTarget::file(
            "feature-owner",
            "file-two",
            "two.wav",
            stage,
            "features-v1",
            1,
            "content-two",
        ));
    }
    targets.push(ReadinessTarget::source(
        "feature-owner",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-one",
    ));
    let mut store = fixture.database.readiness_store().unwrap();
    let origin = store.capture_publication_origin().unwrap();
    store
        .publish_targets(
            &origin,
            &ReadinessTargetPublication::new(
                "feature-owner",
                1,
                1,
                SourceAvailability::Active,
                "features-contract-v1",
                &targets,
                1,
            ),
        )
        .unwrap();
    let deficits = store.reconcile("feature-owner", 10).unwrap().deficits;
    store.persist_deficits(&deficits, 10).unwrap();
    (fixture, target)
}

fn payload(value: f32) -> FeatureArtifactPayload {
    FeatureArtifactPayload::from_v1_vector(&vec![value; 183]).unwrap()
}

pub(super) fn owner(fixture: &mut CurationFixture) -> SourceWriter {
    let file: String = fixture
        .database
        .connection
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let observer = SourceDatabase::open_for_ui_read_with_database_root(
        fixture._source.path(),
        Path::new(&file).parent().unwrap(),
    )
    .unwrap();
    let db = std::mem::replace(&mut fixture.database, observer);
    SourceWriter::new(db, SourceId::from_string("feature-owner"), 7).unwrap()
}

fn count(writer: &SourceWriter) -> i64 {
    writer
        .catalog_database()
        .connection
        .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn feature_artifact_commits_bytes_and_exact_readiness_then_replays_without_overwrite() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let vector = payload(0.25);
    let cancel = AtomicBool::new(false);
    let FeatureArtifactPublication::Recorded(key) = writer
        .publish_feature_artifact(&revision, &claim, &vector, 11, &cancel)
        .unwrap()
    else {
        panic!("recorded")
    };
    assert_eq!(count(&writer), 1);
    assert_eq!(
        writer.lookup_feature_artifact(&revision, &key).unwrap(),
        Some(vector.clone())
    );
    let stored: String = writer
        .catalog_database()
        .connection
        .query_row(
            "SELECT artifact_ref FROM source_readiness_artifacts WHERE stage='analysis_features'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, key.reference());
    assert_eq!(
        writer
            .publish_feature_artifact(&revision, &claim, &vector, 200, &cancel)
            .unwrap(),
        FeatureArtifactPublication::Duplicate(key.clone())
    );
    assert!(
        writer
            .publish_feature_artifact(&revision, &claim, &payload(0.5), 12, &cancel)
            .is_err()
    );
    assert_eq!(
        writer.lookup_feature_artifact(&revision, &key).unwrap(),
        Some(vector)
    );
    assert_eq!(writer.current_revision().unwrap(), revision);
}

#[test]
fn feature_artifact_insert_failure_rolls_back_completion_and_payload() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    writer.catalog_database().connection.execute_batch("CREATE TRIGGER fail_feature BEFORE INSERT ON source_feature_artifacts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
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
    assert_eq!(count(&writer), 0);
    let (state, completions): (String, i64) = writer.catalog_database().connection.query_row("SELECT status, (SELECT count(*) FROM source_readiness_artifacts) FROM analysis_jobs WHERE readiness_stage='analysis_features' AND readiness_scope_id='file-one'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((state.as_str(), completions), ("running", 0));
}

#[test]
fn feature_artifact_cancel_and_expiry_preserve_retry_without_payload() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 5)
        .unwrap()
        .unwrap();
    assert!(matches!(
        writer.publish_feature_artifact(
            &revision,
            &claim,
            &payload(0.25),
            11,
            &AtomicBool::new(true)
        ),
        Err(SourceWriterError::Readiness(ReadinessError::Cancelled))
    ));
    assert_eq!(
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &payload(0.25),
                15,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::RejectedStale
    );
    assert_eq!(count(&writer), 0);
    assert!(
        writer
            .claim_feature_artifact(&revision, &target, 16, 10)
            .unwrap()
            .is_some()
    );
}

#[test]
fn feature_artifact_changed_content_and_reclaimed_generation_reject_stale() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 5)
        .unwrap()
        .unwrap();
    let next = writer
        .claim_feature_artifact(&revision, &target, 16, 20)
        .unwrap()
        .unwrap();
    assert_eq!(
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &payload(0.25),
                17,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::RejectedStale
    );
    writer.catalog_database().connection.execute("UPDATE source_readiness_targets SET content_generation='changed' WHERE stage='analysis_features'", []).unwrap();
    assert_eq!(
        writer
            .publish_feature_artifact(
                &revision,
                &next,
                &payload(0.25),
                18,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::RejectedStale
    );
    assert_eq!(count(&writer), 0);
}

#[test]
fn feature_artifact_capacity_does_not_complete_claim() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    writer.catalog_database().connection.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO source_feature_artifacts SELECT printf('%064d',i), printf('seed:%d',i), zeroblob(732), zeroblob(32), 1 FROM n;").unwrap();
    assert!(matches!(
        writer.publish_feature_artifact(
            &revision,
            &claim,
            &payload(0.25),
            11,
            &AtomicBool::new(false)
        ),
        Err(SourceWriterError::Readiness(
            ReadinessError::FeatureArtifactCapacity
        ))
    ));
    let state: String = writer
        .catalog_database()
        .connection
        .query_row(
            "SELECT status FROM analysis_jobs WHERE readiness_stage='analysis_features' AND readiness_scope_id='file-one'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "running");
}

#[test]
fn feature_artifact_restart_revalidates_lookup_and_rejects_old_claim() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let vector = payload(0.25);
    let FeatureArtifactPublication::Recorded(key) = writer
        .publish_feature_artifact(&revision, &claim, &vector, 11, &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("recorded")
    };
    drop(writer);
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let mut writer =
        SourceWriter::new(database, SourceId::from_string("feature-owner"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    assert_eq!(
        writer
            .lookup_feature_artifact_for_target(&revision, &target)
            .unwrap(),
        Some((key.clone(), vector.clone()))
    );
    assert!(
        writer
            .publish_feature_artifact(&revision, &claim, &vector, 12, &AtomicBool::new(false))
            .is_err()
    );
}

#[test]
fn feature_artifact_corrupt_or_missing_payload_is_unavailable() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let FeatureArtifactPublication::Recorded(key) = writer
        .publish_feature_artifact(
            &revision,
            &claim,
            &payload(0.25),
            11,
            &AtomicBool::new(false),
        )
        .unwrap()
    else {
        panic!("recorded")
    };
    writer
        .catalog_database()
        .connection
        .execute(
            "UPDATE source_feature_artifacts SET payload=zeroblob(732)",
            [],
        )
        .unwrap();
    assert_eq!(
        writer.lookup_feature_artifact(&revision, &key).unwrap(),
        None
    );
    writer
        .catalog_database()
        .connection
        .execute("DELETE FROM source_feature_artifacts", [])
        .unwrap();
    assert_eq!(
        writer.lookup_feature_artifact(&revision, &key).unwrap(),
        None
    );
}

#[test]
fn feature_artifact_vector_validation_rejects_shape_nan_and_infinity() {
    for vector in [
        vec![0.0; 182],
        vec![f32::NAN; 183],
        vec![f32::INFINITY; 183],
    ] {
        assert!(FeatureArtifactPayload::from_v1_vector(&vector).is_err());
    }
}

#[test]
fn feature_artifact_new_writer_instance_rejects_old_claim_even_with_live_lease() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let database = writer.into_catalog_database();
    let mut writer =
        SourceWriter::new(database, SourceId::from_string("feature-owner"), 8).unwrap();
    let current = writer.current_revision().unwrap();
    assert!(matches!(
        writer.publish_feature_artifact(
            &current,
            &claim,
            &payload(0.25),
            11,
            &AtomicBool::new(false)
        ),
        Err(SourceWriterError::StaleFence)
    ));
    assert_eq!(count(&writer), 0);
}

#[test]
fn feature_artifact_source_root_replacement_rejects_publication() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let root = fixture._source.path();
    let displaced = root.with_extension("feature-displaced");
    std::fs::rename(root, &displaced).unwrap();
    std::fs::create_dir(root).unwrap();
    let result = writer.publish_feature_artifact(
        &revision,
        &claim,
        &payload(0.25),
        11,
        &AtomicBool::new(false),
    );
    std::fs::remove_dir(root).unwrap();
    std::fs::rename(displaced, root).unwrap();
    assert!(result.is_err());
    assert_eq!(count(&writer), 0);
}

#[test]
fn feature_artifact_stale_revision_rejects_before_completion() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    writer
        .commit_catalog_curation_command(
            &revision,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            }],
        )
        .unwrap();
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
    assert_eq!(count(&writer), 0);
    let current = writer.current_revision().unwrap();
    assert!(matches!(
        writer
            .publish_feature_artifact(
                &current,
                &claim,
                &payload(0.25),
                11,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::Recorded(_)
    ));
}

#[test]
fn feature_artifact_cancelled_claim_cannot_publish_but_new_claim_can() {
    let (mut fixture, target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    writer
        .cancel_readiness_work(&revision, claim.readiness_claim(), "cancelled", 11)
        .unwrap();
    assert_eq!(
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &payload(0.25),
                12,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::RejectedStale
    );
    assert_eq!(count(&writer), 0);
    let next = writer
        .claim_feature_artifact(&revision, &target, 12, 100)
        .unwrap()
        .unwrap();
    assert!(matches!(
        writer
            .publish_feature_artifact(
                &revision,
                &next,
                &payload(0.25),
                13,
                &AtomicBool::new(false)
            )
            .unwrap(),
        FeatureArtifactPublication::Recorded(_)
    ));
}

#[test]
fn feature_artifact_unsupported_algorithm_cannot_claim() {
    let (mut fixture, mut target) = prepare();
    let mut writer = owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    target.required_version = "features-v2".into();
    assert!(matches!(
        writer.claim_feature_artifact(&revision, &target, 10, 100),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    assert_eq!(count(&writer), 0);
}

#[path = "feature_metrics_tests.rs"]
mod metrics;
