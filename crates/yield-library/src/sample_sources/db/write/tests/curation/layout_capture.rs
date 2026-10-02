use super::*;
use crate::sample_sources::db::write::{SourceLayoutCapture, SourceLayoutCaptureError};
use crate::sample_sources::readiness::{
    FeatureArtifactFixtureFault, FeatureArtifactPayload, FeatureArtifactPublication,
    ReadinessClassification, ReadinessDeficit,
};
use std::time::{Duration, Instant};

fn setup() -> (CurationFixture, SourceWriter) {
    let mut fixture = CurationFixture::new_profile_bound();
    let mut batch = fixture.database.write_batch().unwrap();
    for name in ["one.wav", "two.wav"] {
        let path = fixture._source.path().join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        let metadata = path.metadata().unwrap();
        let identity =
            crate::filesystem_identity::stable_filesystem_identity(&path, &metadata).unwrap();
        batch
            .upsert_file_with_hash(
                Path::new(name),
                metadata.len(),
                1,
                &blake3::hash(name.as_bytes()).to_hex(),
            )
            .unwrap();
        batch
            .set_file_identity(Path::new(name), Some(&identity))
            .unwrap();
    }
    batch.commit().unwrap();
    let revision = fixture.database.get_revision().unwrap();
    fixture
        .database
        .begin_source_directory_truth_generation(2, 0)
        .unwrap();
    fixture
        .database
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            2,
            revision,
            20,
            fixture.root_identity.clone(),
            None,
        )
        .unwrap();
    let observer = SourceDatabase::open_for_ui_read_with_database_root(
        fixture._source.path(),
        &fixture
            .profile_guard
            .profile_root()
            .join("curation-profile-bound"),
    )
    .unwrap();
    let database = std::mem::replace(&mut fixture.database, observer);
    let writer = SourceWriter::new(database, SourceId::from_string("layout-features"), 7).unwrap();
    (fixture, writer)
}
fn publish(writer: &mut SourceWriter, names: &[&str]) -> Vec<ReadinessTarget> {
    let revision = writer.current_revision().unwrap();
    let first = writer
        .prepare_feature_readiness_target(
            &revision,
            Path::new(names[0]),
            30,
            &AtomicBool::new(false),
        )
        .unwrap();
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let (identity, content): (String, String) = writer
                .database_for_tests()
                .connection
                .query_row(
                    "SELECT file_identity,content_hash FROM wav_files WHERE path=?1",
                    [name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let target = ReadinessTarget::file(
                &first.source_id,
                identity,
                *name,
                ReadinessStage::AnalysisFeatures,
                "features-v1",
                first.source_generation,
                content,
            );
            writer
                .enqueue_readiness_jobs(
                    &revision,
                    &[ReadinessDeficit {
                        target: target.clone(),
                        reason: ReadinessClassification::Pending,
                        enqueued_at: None,
                    }],
                    30,
                )
                .unwrap();
            let claim = writer
                .claim_feature_artifact(&revision, &target, 31, 100)
                .unwrap()
                .unwrap();
            let payload =
                FeatureArtifactPayload::from_v1_vector(&[0.25 + index as f32; 183]).unwrap();
            assert!(matches!(
                writer
                    .publish_feature_artifact(
                        &revision,
                        &claim,
                        &payload,
                        32,
                        &AtomicBool::new(false)
                    )
                    .unwrap(),
                FeatureArtifactPublication::Recorded(_)
            ));
            target
        })
        .collect()
}
fn capture(writer: &SourceWriter) -> Result<SourceLayoutCapture, SourceLayoutCaptureError> {
    writer.capture_layout_features(
        &writer.current_revision().unwrap(),
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(2),
    )
}
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
fn layout_capture_real_payloads_are_complete_ordered_bounded_and_read_only() {
    let (_fixture, mut writer) = setup();
    let targets = publish(&mut writer, &["two.wav", "one.wav"]);
    let before = persisted(&writer);
    let changes = writer.database_for_tests().connection.total_changes();
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", true)
        .unwrap();
    let observed = capture(&writer).unwrap();
    assert_eq!(
        observed
            .features()
            .iter()
            .map(|row| row.path())
            .collect::<Vec<_>>(),
        ["one.wav", "two.wav"]
    );
    assert_eq!(
        observed.features()[0].sample_identity(),
        "layout-features::one.wav"
    );
    assert_eq!(observed.features()[0].target(), &targets[1]);
    assert_eq!(
        observed.features()[0].payload().bytes(),
        FeatureArtifactPayload::from_v1_vector(&[1.25; 183])
            .unwrap()
            .bytes()
    );
    assert!(observed.owned_bytes() <= 1024 * 1024);
    let repeat = capture(&writer).unwrap();
    assert_eq!(observed.input_digest(), repeat.input_digest());
    observed.validate_read_fence().unwrap();
    writer
        .validate_layout_capture(
            &observed,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", false)
        .unwrap();
    writer.close().unwrap();
    assert!(
        observed.validate_read_fence().is_err(),
        "capture kept an ended writer alive"
    );
}

#[test]
fn layout_capture_missing_corrupt_and_unconfirmed_inputs_retain_exact_evidence() {
    let (_fixture, mut writer) = setup();
    let absent = capture(&writer).unwrap_err();
    assert!(matches!(
        absent,
        SourceLayoutCaptureError::FeatureUnavailable {
            reason: "layout feature target unavailable or unsupported",
            ..
        }
    ));
    let targets = publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    for (fault, expected) in [
        (
            FeatureArtifactFixtureFault::CorruptPayload,
            "layout feature payload corrupt",
        ),
        (
            FeatureArtifactFixtureFault::MissingPayload,
            "layout feature payload missing",
        ),
    ] {
        writer
            .inject_feature_artifact_fixture_fault(observed.revision(), &targets[0], fault)
            .unwrap();
        let before = persisted(&writer);
        assert!(
            matches!(capture(&writer).unwrap_err(), SourceLayoutCaptureError::FeatureUnavailable { path,reason } if path=="one.wav" && reason==expected)
        );
        assert!(
            writer
                .validate_layout_capture(
                    &observed,
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_secs(2)
                )
                .is_err()
        );
        assert_eq!(persisted(&writer), before);
    }
}

#[test]
fn layout_capture_near_expiry_does_not_renew_and_expired_owner_fails_without_writes() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 30],
        )
        .unwrap();
    let before = persisted(&writer);
    let observed = capture(&writer).unwrap();
    writer
        .validate_layout_capture(
            &observed,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
            [],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(
        writer
            .capture_layout_features(
                observed.revision(),
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        writer
            .validate_layout_capture(
                &observed,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_empty_cancelled_expired_deadline_and_unsupported_format_are_explicit() {
    let (_fixture, writer) = setup();
    let revision = writer.current_revision().unwrap();
    assert!(matches!(
        writer.capture_layout_features(
            &revision,
            &AtomicBool::new(true),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutCaptureError::Cancelled)
    ));
    assert!(matches!(
        writer.capture_layout_features(&revision, &AtomicBool::new(false), Instant::now()),
        Err(SourceLayoutCaptureError::Deadline)
    ));
    writer
        .database_for_tests()
        .connection
        .execute("UPDATE wav_files SET missing=1", [])
        .unwrap();
    let before = persisted(&writer);
    assert!(matches!(
        capture(&writer),
        Err(SourceLayoutCaptureError::Empty)
    ));
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET missing=0,path='one.flac',extension='wav' WHERE path='one.wav'",
            [],
        )
        .unwrap();
    assert!(
        matches!(capture(&writer),Err(SourceLayoutCaptureError::UnsupportedFormat(path)) if path=="one.flac")
    );
}

#[test]
fn layout_capture_rejects_aliases_duplicate_identity_invalid_types_and_oversized_fields() {
    for statement in [
        "UPDATE wav_files SET path='one//sub.wav' WHERE path='one.wav'",
        "UPDATE wav_files SET path='./one.wav' WHERE path='one.wav'",
        "UPDATE wav_files SET path='dir\\one.wav' WHERE path='one.wav'",
        "UPDATE wav_files SET path='../one.wav' WHERE path='one.wav'",
        "UPDATE wav_files SET file_identity=(SELECT file_identity FROM wav_files WHERE path='two.wav') WHERE path='one.wav'",
        "UPDATE wav_files SET file_identity=zeroblob(512) WHERE path='one.wav'",
        "UPDATE wav_files SET content_hash=upper(content_hash) WHERE path='one.wav'",
        "UPDATE wav_files SET content_hash='abbreviated' WHERE path='one.wav'",
        "UPDATE wav_files SET file_size=1.5 WHERE path='one.wav'",
        "UPDATE wav_files SET modified_ns=zeroblob(1) WHERE path='one.wav'",
        "UPDATE wav_files SET missing='unknown' WHERE path='one.wav'",
        "UPDATE wav_files SET missing=2 WHERE path='one.wav'",
        "UPDATE wav_files SET file_identity=printf('%0513d',1) WHERE path='one.wav'",
        "UPDATE wav_files SET path=printf('%01025d',1)||'.wav' WHERE path='one.wav'",
    ] {
        let (_fixture, writer) = setup();
        writer
            .database_for_tests()
            .connection
            .execute(statement, [])
            .unwrap();
        let before = persisted(&writer);
        assert!(
            matches!(
                capture(&writer),
                Err(SourceLayoutCaptureError::InvalidManifest(_))
            ),
            "{statement}"
        );
        assert_eq!(persisted(&writer), before, "{statement}");
    }
}

fn additional_rows(writer: &SourceWriter, count: usize, path_bytes: usize, identity_bytes: usize) {
    for index in 0..count {
        let path = format!(
            "{:0width$}.wav",
            index,
            width = path_bytes.saturating_sub(4)
        );
        let identity = format!("{:0width$}", index, width = identity_bytes);
        writer.database_for_tests().connection.execute(
            "INSERT INTO wav_files(path,file_size,modified_ns,extension,file_identity,content_hash) VALUES(?1,1,1,'wav',?2,?3)",
            rusqlite::params![path,identity,blake3::hash(path.as_bytes()).to_hex().to_string()]).unwrap();
    }
}

#[test]
fn layout_capture_257th_row_and_aggregate_bytes_reject_before_any_feature_result() {
    let (_fixture, writer) = setup();
    additional_rows(&writer, 255, 20, 20);
    let before = persisted(&writer);
    assert!(matches!(
        capture(&writer),
        Err(SourceLayoutCaptureError::Limit("manifest rows"))
    ));
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .execute(
            "DELETE FROM wav_files WHERE path='0000000000000000.wav'",
            [],
        )
        .unwrap();
    // Exact 256 is admitted through manifest validation; missing features remain explicit.
    assert!(matches!(
        capture(&writer),
        Err(SourceLayoutCaptureError::FeatureUnavailable { .. })
    ));
    writer.close().unwrap();
    let (_fixture, writer) = setup();
    additional_rows(&writer, 128, 1000, 480);
    let before = persisted(&writer);
    assert!(matches!(
        capture(&writer),
        Err(SourceLayoutCaptureError::Limit("manifest bytes"))
    ));
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_revalidates_full_membership_content_version_and_manifest_facts() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET file_size=file_size+1 WHERE path='one.wav'",
            [],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(matches!(
        writer.validate_layout_capture(
            &observed,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutCaptureError::Stale)
    ));
    assert_ne!(
        capture(&writer).unwrap().input_digest(),
        observed.input_digest()
    );
    assert_eq!(persisted(&writer), before);
    writer.database_for_tests().connection.execute("UPDATE source_readiness_targets SET required_version='features-v2' WHERE stage='analysis_features'",[]).unwrap();
    let before = persisted(&writer);
    assert!(matches!(
        capture(&writer),
        Err(SourceLayoutCaptureError::FeatureUnavailable {
            reason: "layout feature target unavailable or unsupported",
            ..
        })
    ));
    assert_eq!(persisted(&writer), before);
    writer.database_for_tests().connection.execute("UPDATE source_readiness_targets SET required_version='features-v1' WHERE stage='analysis_features'",[]).unwrap();
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET content_hash=?1 WHERE path='one.wav'",
            [blake3::hash(b"changed").to_hex().to_string()],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(capture(&writer).is_err());
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_restart_reissues_current_keys_without_reusing_old_incarnation() {
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    writer.close().unwrap();
    assert!(observed.validate_read_fence().is_err());
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let writer = SourceWriter::new(database, SourceId::from_string("layout-features"), 8).unwrap();
    let before = persisted(&writer);
    assert!(matches!(
        writer.validate_layout_capture(
            &observed,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutCaptureError::Stale)
    ));
    let current = capture(&writer).unwrap();
    assert_eq!(current.input_digest(), observed.input_digest());
    assert_ne!(current.revision(), observed.revision());
    assert_eq!(current.features()[0].key(), observed.features()[0].key());
    writer
        .validate_layout_capture(
            &current,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_ambiguous_provenance_and_missing_completion_are_not_repaired() {
    for (sql, expected) in [
        (
            "UPDATE source_feature_artifacts SET artifact_ref='unknown-owner-reference'",
            "layout feature provenance ambiguous",
        ),
        (
            "DELETE FROM source_readiness_artifacts WHERE stage='analysis_features'",
            "layout feature completion missing",
        ),
    ] {
        let (_fixture, mut writer) = setup();
        publish(&mut writer, &["one.wav", "two.wav"]);
        // Make only one row ambiguous to preserve the real UNIQUE reference constraint.
        let sql = if sql.starts_with("UPDATE") {
            format!(
                "{sql} WHERE target_key=(SELECT target_key FROM source_feature_artifacts ORDER BY target_key LIMIT 1)"
            )
        } else {
            sql.to_owned()
        };
        writer
            .database_for_tests()
            .connection
            .execute(&sql, [])
            .unwrap();
        let before = persisted(&writer);
        assert!(
            matches!(capture(&writer),Err(SourceLayoutCaptureError::FeatureUnavailable {reason,..}) if reason==expected)
        );
        assert_eq!(persisted(&writer), before);
    }
}

#[test]
fn layout_capture_changed_audited_revision_and_physical_owner_scope_reject_old_input() {
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    // A genuine metadata-only owner commit changes the audited revision without
    // changing any layout input. The historical capture expires; a fresh one must
    // retain the reusable digest and actual feature keys without publishing again.
    let receipt = writer
        .commit_catalog_curation_command(
            observed.revision(),
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            }],
        )
        .unwrap();
    assert!(receipt.source_revision() > observed.revision().revision());
    let before = persisted(&writer);
    assert!(observed.validate_read_fence().is_err());
    let current = capture(&writer).unwrap();
    assert_eq!(current.revision().revision(), receipt.source_revision());
    assert_eq!(current.input_digest(), observed.input_digest());
    for (new, old) in current.features().iter().zip(observed.features()) {
        assert_eq!(new.key(), old.key());
    }
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE metadata SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",
            [],
        )
        .unwrap();
    // A semantic revision change cannot be admitted from a now-outdated complete audit.
    let before = persisted(&writer);
    assert!(
        writer
            .capture_layout_features(
                observed.revision(),
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_maximum_complete_manifest_uses_actual_published_features() {
    let (fixture, mut writer) = setup();
    let names: Vec<String> = (0..254)
        .map(|index| format!("extra-{index:03}.wav"))
        .collect();
    let mut batch = writer.database_for_tests().write_batch().unwrap();
    for name in &names {
        let path = fixture._source.path().join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        let metadata = path.metadata().unwrap();
        let identity =
            crate::filesystem_identity::stable_filesystem_identity(&path, &metadata).unwrap();
        batch
            .upsert_file_with_hash(
                Path::new(name),
                metadata.len(),
                1,
                &blake3::hash(name.as_bytes()).to_hex(),
            )
            .unwrap();
        batch
            .set_file_identity(Path::new(name), Some(&identity))
            .unwrap();
    }
    batch.commit().unwrap();
    let revision = writer.database_for_tests().get_revision().unwrap();
    writer
        .database_for_tests()
        .begin_source_directory_truth_generation(3, 0)
        .unwrap();
    writer
        .database_for_tests()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            3,
            revision,
            25,
            fixture.root_identity.clone(),
            None,
        )
        .unwrap();
    let mut all: Vec<&str> = names.iter().map(String::as_str).collect();
    all.extend(["one.wav", "two.wav"]);
    publish(&mut writer, &all);
    let before = persisted(&writer);
    let start = Instant::now();
    let observed = capture(&writer).unwrap();
    assert_eq!(observed.features().len(), SourceLayoutCapture::MAX_FILES);
    assert!(observed.owned_bytes() <= SourceLayoutCapture::MAX_INPUT_BYTES);
    assert!(
        observed
            .features()
            .windows(2)
            .all(|pair| pair[0].path() < pair[1].path())
    );
    assert_eq!(persisted(&writer), before);
    eprintln!(
        "actual 256-row feature capture: {:?}, charged {} bytes",
        start.elapsed(),
        observed.owned_bytes()
    );
}

#[cfg(unix)]
#[test]
fn layout_capture_rejects_replaced_physical_root_and_retains_database_bytes() {
    struct RestoreRoot {
        original: PathBuf,
        moved: PathBuf,
    }
    impl Drop for RestoreRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.original);
            let _ = std::fs::rename(&self.moved, &self.original);
        }
    }
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    let original = fixture._source.path().to_path_buf();
    let moved = original.with_extension("layout-original");
    std::fs::rename(&original, &moved).unwrap();
    let _restore = RestoreRoot {
        original: original.clone(),
        moved,
    };
    std::fs::create_dir(&original).unwrap();
    let before = persisted(&writer);
    assert!(observed.validate_read_fence().is_err());
    assert!(
        writer
            .capture_layout_features(
                observed.revision(),
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        writer
            .validate_layout_capture(
                &observed,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert_eq!(persisted(&writer), before);
}

#[test]
fn layout_capture_public_input_never_exposes_uncommitted_manifest_facts() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    writer
        .database_for_tests()
        .connection
        .execute_batch(
            "BEGIN IMMEDIATE; UPDATE wav_files SET file_size=file_size+1 WHERE path='one.wav';",
        )
        .unwrap();
    let pending = writer.capture_layout_features(
        observed.revision(),
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(2),
    );
    writer
        .database_for_tests()
        .connection
        .execute_batch("ROLLBACK")
        .unwrap();
    assert!(
        matches!(pending, Err(SourceLayoutCaptureError::TransactionActive)),
        "public CPU input exposed uncommitted manifest facts"
    );
    assert_eq!(
        capture(&writer).unwrap().input_digest(),
        observed.input_digest()
    );
}

#[test]
fn layout_capture_never_omits_audio_when_persisted_classification_disagrees_with_path() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let observed = capture(&writer).unwrap();
    for value in ["flac", "WAV", ""] {
        writer
            .database_for_tests()
            .connection
            .execute(
                "UPDATE wav_files SET extension=?1 WHERE path='one.wav'",
                [value],
            )
            .unwrap();
        let before = persisted(&writer);
        assert!(
            matches!(
                capture(&writer),
                Err(SourceLayoutCaptureError::InvalidManifest(
                    "audio classification mismatch"
                ))
            ),
            "corrupt classification {value:?} silently omitted a captured member"
        );
        assert!(
            writer
                .validate_layout_capture(
                    &observed,
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_secs(2)
                )
                .is_err()
        );
        assert_eq!(persisted(&writer), before);
    }
}
