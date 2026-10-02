use super::*;
use crate::sample_sources::readiness::{FeatureArtifactPayload, FeatureArtifactPublication};

fn persisted(writer: &SourceWriter) -> Vec<(PathBuf, Vec<u8>)> {
    let file = PathBuf::from(writer.database_for_tests().connection.path().unwrap());
    [
        file.clone(),
        PathBuf::from(format!("{}-wal", file.display())),
    ]
    .into_iter()
    .filter_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
    .collect()
}

#[test]
fn feature_lookup_read_only_never_renews_near_expiry_or_expired_lease() {
    let (mut fixture, target) = feature_artifacts::prepare();
    let mut writer = feature_artifacts::owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let claim = writer
        .claim_feature_artifact(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    let payload = FeatureArtifactPayload::from_v1_vector(&[0.25; 183]).unwrap();
    let FeatureArtifactPublication::Recorded(key) = writer
        .publish_feature_artifact(&revision, &claim, &payload, 11, &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("recorded")
    };
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    // Still valid, but within the real mutation-renewal threshold. A read must not
    // use mutation qualification through the separate lease control connection.
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 30],
        )
        .unwrap();
    let before = persisted(&writer);
    let changes = writer.database_for_tests().connection.total_changes();
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", true)
        .unwrap();
    assert_eq!(
        writer.lookup_feature_artifact(&revision, &key).unwrap(),
        Some(payload.clone())
    );
    assert_eq!(
        writer
            .lookup_feature_artifact_for_target(&revision, &target)
            .unwrap(),
        Some((key.clone(), payload))
    );
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert!(
        persisted(&writer) == before,
        "read renewed lease or changed persistent bytes"
    );
    let expiry: i64 = writer
        .database_for_tests()
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(expiry, now + 30);
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", false)
        .unwrap();
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
            [],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(writer.lookup_feature_artifact(&revision, &key).is_err());
    assert!(
        writer
            .lookup_feature_artifact_for_target(&revision, &target)
            .is_err()
    );
    assert!(persisted(&writer) == before);
    writer.close().unwrap();
}
