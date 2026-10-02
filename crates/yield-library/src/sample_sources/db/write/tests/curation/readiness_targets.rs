#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::sample_sources::readiness::{
        FeatureArtifactPayload, FeatureArtifactPublication, ReadinessClassification,
        ReadinessDeficit, ReadinessEligibility,
    };

    fn setup() -> (CurationFixture, SourceWriter) {
        let mut f = CurationFixture::new_profile_bound();
        // Real temporary-file bytes, full BLAKE3 and platform identity. The retained
        // fixture writer commits these observations and then a complete audit.
        let mut batch = f.database.write_batch().unwrap();
        for name in ["one.wav", "two.wav"] {
            let path = f._source.path().join(name);
            std::fs::write(&path, name.as_bytes()).unwrap();
            let meta = path.metadata().unwrap();
            let identity =
                crate::filesystem_identity::stable_filesystem_identity(&path, &meta).unwrap();
            let hash = blake3::hash(name.as_bytes()).to_hex().to_string();
            batch
                .upsert_file_with_hash(Path::new(name), meta.len(), 1, &hash)
                .unwrap();
            batch
                .set_file_identity(Path::new(name), Some(&identity))
                .unwrap();
        }
        batch.commit().unwrap();
        let revision = f.database.get_revision().unwrap();
        f.database
            .begin_source_directory_truth_generation(2, 0)
            .unwrap();
        f.database
            .write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(
                2,
                revision,
                20,
                f.root_identity.clone(),
                None,
            )
            .unwrap();
        let observer = SourceDatabase::open_for_ui_read_with_database_root(
            f._source.path(),
            &f.profile_guard
                .profile_root()
                .join("curation-profile-bound"),
        )
        .unwrap();
        let db = std::mem::replace(&mut f.database, observer);
        let writer = SourceWriter::new(db, SourceId::from_string("manifest-features"), 7).unwrap();
        (f, writer)
    }

    fn publish(writer: &mut SourceWriter) -> ReadinessTarget {
        let revision = writer.current_revision().unwrap();
        writer
            .prepare_feature_readiness_target(
                &revision,
                Path::new("one.wav"),
                30,
                &AtomicBool::new(false),
            )
            .unwrap()
    }
    fn counts(writer: &SourceWriter) -> (i64, i64, i64) {
        writer
            .catalog_database()
            .connection
            .query_row(
                "SELECT (SELECT count(*) FROM source_readiness_targets),
             (SELECT count(*) FROM source_readiness_artifacts),
             (SELECT count(*) FROM source_readiness_sources)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    #[test]
    fn manifest_readiness_publishes_complete_matrix_index_evidence_and_real_feature_claim() {
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        assert_eq!(counts(&writer), (7, 2, 1));
        assert_eq!(target.required_version, "features-v1");
        assert_eq!(target.relative_path.as_deref(), Some("one.wav"));
        let store = crate::sample_sources::readiness::ReadinessView::new(
            &writer.catalog_database().connection,
        );
        let snapshot = store.reconcile("manifest-features", 30).unwrap();
        assert!(
            snapshot
                .entries
                .iter()
                .filter(|e| e.target.stage == ReadinessStage::IndexedIdentity)
                .all(|e| matches!(e.classification, ReadinessClassification::Current))
        );
        assert!(
            snapshot
                .entries
                .iter()
                .filter(|e| matches!(
                    e.target.stage,
                    ReadinessStage::EmbeddingAspects | ReadinessStage::SimilarityLayout
                ))
                .all(|e| e.target.eligibility == ReadinessEligibility::Unsupported)
        );
        let revision = writer.current_revision().unwrap();
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
        // Payload fixture verifies owned SQL ordering, not DSP/physical qualification.
        let payload = FeatureArtifactPayload::from_v1_vector(&vec![0.5; 183]).unwrap();
        assert!(matches!(
            writer
                .publish_feature_artifact(&revision, &claim, &payload, 32, &AtomicBool::new(false))
                .unwrap(),
            FeatureArtifactPublication::Recorded(_)
        ));
        let next = publish(&mut writer);
        assert_eq!(next, target);
        assert_eq!(counts(&writer), (7, 3, 1));
        assert_eq!(
            writer
                .lookup_feature_artifact_for_target(&revision, &next)
                .unwrap()
                .unwrap()
                .1,
            payload
        );
    }

    #[test]
    fn manifest_readiness_new_audit_content_gets_new_target_without_adopting_old_payload() {
        let (f, mut writer) = setup();
        let original = publish(&mut writer);
        let revision = writer.current_revision().unwrap();
        writer
            .enqueue_readiness_jobs(
                &revision,
                &[ReadinessDeficit {
                    target: original.clone(),
                    reason: ReadinessClassification::Pending,
                    enqueued_at: None,
                }],
                30,
            )
            .unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, &original, 31, 100)
            .unwrap()
            .unwrap();
        let payload = FeatureArtifactPayload::from_v1_vector(&vec![0.5; 183]).unwrap();
        writer
            .publish_feature_artifact(&revision, &claim, &payload, 32, &AtomicBool::new(false))
            .unwrap();
        let db = writer.into_catalog_database();
        let content = b"new actual file bytes";
        std::fs::write(f._source.path().join("one.wav"), content).unwrap();
        let mut batch = db.write_batch().unwrap();
        batch
            .upsert_file_with_hash(
                Path::new("one.wav"),
                content.len() as u64,
                2,
                &blake3::hash(content).to_hex().to_string(),
            )
            .unwrap();
        batch.commit().unwrap();
        db.begin_source_directory_truth_generation(3, 0).unwrap();
        db.write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(
                3,
                db.get_revision().unwrap(),
                40,
                f.root_identity.clone(),
                None,
            )
            .unwrap();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 7).unwrap();
        assert!(matches!(
            writer.prepare_feature_readiness_target(
                &revision,
                Path::new("one.wav"),
                40,
                &AtomicBool::new(false)
            ),
            Err(SourceWriterError::StaleRevision { .. })
        ));
        let current = publish(&mut writer);
        assert_ne!(current.content_generation, original.content_generation);
        assert!(current.source_generation > original.source_generation);
        let revision = writer.current_revision().unwrap();
        assert!(
            writer
                .lookup_feature_artifact_for_target(&revision, &current)
                .unwrap()
                .is_none()
        );
        let view = crate::sample_sources::readiness::ReadinessView::new(
            &writer.catalog_database().connection,
        );
        assert!(
            view.reconcile("manifest-features", 40)
                .unwrap()
                .entries
                .iter()
                .filter(|e| e.target == current)
                .all(|e| e.classification != ReadinessClassification::Current)
        );
    }

    #[test]
    fn manifest_readiness_invalid_manifest_values_never_publish_partial_targets() {
        for sql in [
            "UPDATE wav_files SET content_hash=NULL WHERE path='two.wav'",
            "UPDATE wav_files SET content_hash='pending-deep-hash' WHERE path='two.wav'",
            "UPDATE wav_files SET content_hash=upper(content_hash) WHERE path='two.wav'",
            "UPDATE wav_files SET file_identity=NULL WHERE path='two.wav'",
            "UPDATE wav_files SET file_identity=(SELECT file_identity FROM wav_files WHERE path='one.wav') WHERE path='two.wav'",
            "UPDATE wav_files SET file_identity=printf('%0600d',1) WHERE path='two.wav'",
            "UPDATE wav_files SET path=printf('%02000d',1)||'.wav' WHERE path='two.wav'",
        ] {
            let (_f, mut writer) = setup();
            let revision = writer.current_revision().unwrap();
            writer
                .catalog_database()
                .connection
                .execute(sql, [])
                .unwrap();
            assert!(
                writer
                    .prepare_feature_readiness_target(
                        &revision,
                        Path::new("one.wav"),
                        30,
                        &AtomicBool::new(false)
                    )
                    .is_err(),
                "{sql}"
            );
            assert_eq!(counts(&writer), (0, 0, 0));
        }
    }

    #[test]
    fn manifest_readiness_rejects_row_and_total_byte_limits_before_publication() {
        for large_bytes in [false, true] {
            let (_f, mut writer) = setup();
            let revision = writer.current_revision().unwrap();
            for i in 0..=SourceWriter::FEATURE_READINESS_MAX_FILES {
                let name = if large_bytes {
                    format!("{}-{i}.wav", "p".repeat(990))
                } else {
                    format!("extra-{i}.wav")
                };
                writer.catalog_database().connection.execute(
                    "INSERT INTO wav_files(path,file_size,modified_ns,file_identity,content_hash,missing,extension) VALUES (?1,1,1,?2,?3,0,'wav')",
                    rusqlite::params![name, format!("extra-{i}"), "a".repeat(64)]).unwrap();
            }
            let error = writer
                .prepare_feature_readiness_target(
                    &revision,
                    Path::new("one.wav"),
                    30,
                    &AtomicBool::new(false),
                )
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    SourceWriterError::InvalidRequest("feature manifest byte limit")
                ) == large_bytes,
                "{error:?}"
            );
            assert_eq!(counts(&writer), (0, 0, 0));
        }
    }

    #[test]
    fn manifest_readiness_cancellation_stale_or_incomplete_audit_never_publish() {
        for corrupt in [false, true] {
            let (_f, mut writer) = setup();
            let revision = writer.current_revision().unwrap();
            if corrupt {
                writer
                    .catalog_database()
                    .connection
                    .execute(
                        "DELETE FROM metadata WHERE key='last_authoritative_audit_revision_v1'",
                        [],
                    )
                    .unwrap();
            }
            assert!(
                writer
                    .prepare_feature_readiness_target(
                        &revision,
                        Path::new("one.wav"),
                        30,
                        &AtomicBool::new(!corrupt)
                    )
                    .is_err()
            );
            assert_eq!(counts(&writer), (0, 0, 0));
        }
        let (_f, mut writer) = setup();
        let revision = writer.current_revision().unwrap();
        writer.catalog_database().connection.execute("UPDATE metadata SET value='999' WHERE key IN ('revision','last_authoritative_audit_revision_v1')", []).unwrap();
        assert!(matches!(
            writer.prepare_feature_readiness_target(
                &revision,
                Path::new("one.wav"),
                30,
                &AtomicBool::new(false)
            ),
            Err(SourceWriterError::StaleRevision { .. })
        ));
        assert_eq!(counts(&writer), (0, 0, 0));
    }

    #[test]
    fn manifest_readiness_transaction_rolls_back_index_targets_and_wake_together() {
        let (_f, mut writer) = setup();
        let revision = writer.current_revision().unwrap();
        writer.catalog_database().connection.execute_batch(
            "CREATE TRIGGER reject_index BEFORE INSERT ON source_readiness_artifacts
             WHEN NEW.stage='indexed_identity' BEGIN SELECT RAISE(ABORT,'fixture index failure'); END;").unwrap();
        assert!(
            writer
                .prepare_feature_readiness_target(
                    &revision,
                    Path::new("one.wav"),
                    30,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(counts(&writer), (0, 0, 0));
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT count(*) FROM source_readiness_wakes", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_readiness_root_replacement_and_incompatible_contract_fail_closed() {
        let (f, mut writer) = setup();
        let revision = writer.current_revision().unwrap();
        let moved = f._source.path().with_extension("replaced");
        std::fs::rename(f._source.path(), &moved).unwrap();
        std::fs::create_dir(f._source.path()).unwrap();
        assert!(
            writer
                .prepare_feature_readiness_target(
                    &revision,
                    Path::new("one.wav"),
                    30,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(counts(&writer), (0, 0, 0));
        std::fs::remove_dir(f._source.path()).unwrap();
        std::fs::rename(&moved, f._source.path()).unwrap();
        drop(writer);
        drop(f);
        let (_f, mut writer) = setup();
        publish(&mut writer);
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_readiness_sources SET contract_version='other-contract'",
                [],
            )
            .unwrap();
        let revision = writer.current_revision().unwrap();
        assert!(matches!(
            writer.prepare_feature_readiness_target(
                &revision,
                Path::new("one.wav"),
                30,
                &AtomicBool::new(false)
            ),
            Err(SourceWriterError::InvalidRequest(
                "incompatible feature readiness contract"
            ))
        ));
        assert_eq!(counts(&writer), (7, 2, 1));
    }
}
