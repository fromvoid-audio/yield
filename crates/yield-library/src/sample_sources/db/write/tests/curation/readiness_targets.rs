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
                .prepare_bounded_feature_readiness_target(
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
    // These extra rows exercise owned SQLite admission, not physical file/engine qualification.
    // The selected one.wav retains its real fixture identity and bytes; a catalog test separately
    // exercises a large manifest produced by the actual scanner and real WAV feature engine.
    fn grow_manifest(
        f: &CurationFixture,
        writer: SourceWriter,
        count: usize,
        long_paths: bool,
    ) -> SourceWriter {
        let db = writer.into_catalog_database();
        let mut batch = db.write_batch().unwrap();
        for i in 0..count {
            let path = if long_paths {
                format!(
                    "{}/{}/{}/extra-{i:05}.wav",
                    "p".repeat(200),
                    "q".repeat(200),
                    "r".repeat(200)
                )
            } else {
                format!("extra-{i:05}.wav")
            };
            batch
                .upsert_file_with_hash(
                    Path::new(&path),
                    1,
                    1,
                    &blake3::hash(path.as_bytes()).to_hex().to_string(),
                )
                .unwrap();
            batch
                .set_file_identity(Path::new(&path), Some(&format!("extra-identity-{i:05}")))
                .unwrap();
        }
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
        SourceWriter::new(db, SourceId::from_string("manifest-features"), 7).unwrap()
    }
    fn stream(
        writer: &mut SourceWriter,
        cancel: &AtomicBool,
        callback: &mut dyn FnMut(usize),
    ) -> Result<ReadinessTarget, SourceWriterError> {
        let revision = writer.current_revision().unwrap();
        writer.stream_feature_readiness_with_checkpoint_for_test(
            &revision,
            Path::new("one.wav"),
            50,
            cancel,
            callback,
        )
    }
    #[test]
    fn streamed_manifest_readiness_exceeds_small_bounds_and_preserves_unchanged_real_payload() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
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
        let payload = FeatureArtifactPayload::from_v1_vector(&vec![0.5; 183]).unwrap();
        writer
            .publish_feature_artifact(&revision, &claim, &payload, 32, &AtomicBool::new(false))
            .unwrap();
        let mut writer = grow_manifest(&f, writer, 1024, true);
        let began = std::time::Instant::now();
        let mut pages = Vec::new();
        let mut peak_input_bytes = 0i64;
        let current = stream(&mut writer, &AtomicBool::new(false), &mut |files| {
            let previous=pages.last().copied().unwrap_or(0);
            let bytes:i64=f.database.connection.query_row(
                "SELECT sum(input_bytes) FROM (SELECT length(CAST(path AS BLOB))+length(CAST(file_identity AS BLOB))+length(CAST(content_hash AS BLOB)) AS input_bytes FROM wav_files WHERE missing=0 ORDER BY path LIMIT ?1 OFFSET ?2)",
                rusqlite::params![(files-previous) as i64,previous as i64],|r|r.get(0)).unwrap();
            peak_input_bytes=peak_input_bytes.max(bytes);
            pages.push(files);
            let visible: (i64,i64)=f.database.connection.query_row(
                "SELECT (SELECT count(*) FROM source_readiness_targets),(SELECT count(*) FROM source_readiness_artifacts)",
                [],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(visible,(7,3)); // actual separate read connection never sees partial pages
        })
        .unwrap();
        eprintln!(
            "streamed owned SQL:1026 files,17 pages, elapsed {:?}; peak manifest input {peak_input_bytes} bytes; page<=64 rows/128KiB",
            began.elapsed()
        );
        assert!(peak_input_bytes > 0 && peak_input_bytes <= 128 * 1024);
        assert_eq!(pages.len(), 17);
        assert!(pages.windows(2).all(|pair| pair[1] - pair[0] <= 64));
        assert_eq!(pages.last(), Some(&1026));
        assert_eq!(counts(&writer), (3079, 1027, 1));
        let revision = writer.current_revision().unwrap();
        assert_eq!(
            writer
                .lookup_feature_artifact_for_target(&revision, &current)
                .unwrap()
                .unwrap()
                .1,
            payload
        );
        assert_eq!(current.content_generation, target.content_generation);
        assert!(current.source_generation > target.source_generation);
        let again = publish(&mut writer); // actual public small-admission fallback to streaming
        assert_eq!(again, current);
        assert_eq!(counts(&writer), (3079, 1027, 1));
        let db = writer.into_catalog_database();
        drop(db);
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            f._source.path(),
            &f.profile_guard,
            Path::new("curation-profile-bound"),
        )
        .unwrap();
        let mut reopened =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 7).unwrap();
        let target = publish(&mut reopened);
        let revision = reopened.current_revision().unwrap();
        assert_eq!(
            reopened
                .lookup_feature_artifact_for_target(&revision, &target)
                .unwrap()
                .unwrap()
                .1,
            payload
        );
    }
    #[test]
    fn streamed_manifest_readiness_cancellation_after_pages_rolls_back_and_retries_after_restart() {
        let (f, mut writer) = setup();
        publish(&mut writer);
        let mut writer = grow_manifest(&f, writer, 300, false);
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            stream(&mut writer, &cancel, &mut |files| {
                if files >= 128 {
                    cancel.store(true, Ordering::Release);
                }
            }),
            Err(SourceWriterError::Readiness(
                crate::sample_sources::readiness::ReadinessError::Cancelled
            ))
        ));
        assert_eq!(counts(&writer), (7, 2, 1));
        let db = writer.into_catalog_database();
        drop(db);
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            f._source.path(),
            &f.profile_guard,
            Path::new("curation-profile-bound"),
        )
        .unwrap();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 7).unwrap();
        let target = publish(&mut writer);
        assert_eq!(target.relative_path.as_deref(), Some("one.wav"));
        assert_eq!(counts(&writer), (907, 302, 1));
    }
    #[test]
    fn streamed_manifest_readiness_root_replacement_between_pages_retains_prior_generation() {
        let (f, mut writer) = setup();
        publish(&mut writer);
        let mut writer = grow_manifest(&f, writer, 300, false);
        let park = tempfile::tempdir().unwrap();
        let mut replaced = false;
        assert!(
            stream(&mut writer, &AtomicBool::new(false), &mut |files| {
                if files >= 128 && !replaced {
                    std::fs::rename(f._source.path(), park.path().join("original")).unwrap();
                    std::fs::create_dir(f._source.path()).unwrap();
                    replaced = true;
                }
            })
            .is_err()
        );
        assert!(replaced);
        assert_eq!(counts(&writer), (7, 2, 1));
    }
    #[test]
    fn streamed_manifest_readiness_late_bad_rows_duplicate_identity_and_trigger_failures_are_atomic()
     {
        for sql in [
            "UPDATE wav_files SET content_hash=NULL WHERE path='extra-00250.wav'",
            "UPDATE wav_files SET content_hash='pending-deep-hash' WHERE path='extra-00250.wav'",
            "UPDATE wav_files SET file_identity=(SELECT file_identity FROM wav_files WHERE path='extra-00000.wav') WHERE path='extra-00250.wav'",
            "CREATE TRIGGER break_stream AFTER INSERT ON source_readiness_targets WHEN NEW.relative_path='extra-00250.wav' BEGIN SELECT RAISE(ABORT,'stream rollback fixture'); END",
            "CREATE TRIGGER break_stream AFTER INSERT ON source_readiness_targets WHEN NEW.relative_path='extra-00250.wav' BEGIN UPDATE metadata SET value='99999' WHERE key='revision'; END",
            "CREATE TRIGGER break_stream AFTER INSERT ON source_readiness_targets WHEN NEW.relative_path='extra-00250.wav' BEGIN UPDATE source_readiness_targets SET content_generation='other-hash' WHERE relative_path='extra-00000.wav'; END",
            "CREATE TRIGGER break_stream AFTER INSERT ON source_readiness_targets WHEN NEW.relative_path='extra-00250.wav' BEGIN DELETE FROM metadata WHERE key='last_authoritative_audit_revision_v1'; END",
        ] {
            let (f, mut writer) = setup();
            publish(&mut writer);
            let mut writer = grow_manifest(&f, writer, 300, false);
            writer
                .catalog_database()
                .connection
                .execute_batch(sql)
                .unwrap();
            assert!(
                stream(&mut writer, &AtomicBool::new(false), &mut |_| {}).is_err(),
                "{sql}"
            );
            assert_eq!(counts(&writer), (7, 2, 1), "{sql}");
            assert!(writer.current_revision().is_ok()); // failed SQL-side changes also rolled back
        }
    }
    #[test]
    fn streamed_manifest_readiness_missing_selected_target_and_incompatible_contract_are_atomic() {
        let (f, mut writer) = setup();
        publish(&mut writer);
        let mut writer = grow_manifest(&f, writer, 300, false);
        let revision = writer.current_revision().unwrap();
        assert!(
            writer
                .prepare_streamed_feature_readiness_target(
                    &revision,
                    Path::new("absent.wav"),
                    50,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(counts(&writer), (7, 2, 1));
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_readiness_sources SET contract_version='another-contract'",
                [],
            )
            .unwrap();
        assert!(stream(&mut writer, &AtomicBool::new(false), &mut |_| {}).is_err());
        assert_eq!(counts(&writer), (7, 2, 1));
    }
    #[test]
    fn streamed_manifest_readiness_elapsed_budget_rolls_back_active_generation() {
        let (f, mut writer) = setup();
        publish(&mut writer);
        let mut writer = grow_manifest(&f, writer, 300, false);
        let error = stream(&mut writer, &AtomicBool::new(false), &mut |files| {
            if files == 64 {
                std::thread::sleep(std::time::Duration::from_millis(2100));
            }
        })
        .unwrap_err();
        assert!(
            matches!(
                error,
                SourceWriterError::Readiness(
                    crate::sample_sources::readiness::ReadinessError::AdmissionBudget(_)
                )
            ),
            "{error:?}"
        );
        assert_eq!(counts(&writer), (7, 2, 1));
        assert!(publish(&mut writer).relative_path.is_some());
    }
    #[test]
    fn streamed_manifest_readiness_foreign_physical_owner_fence_cannot_publish_into_receiver() {
        let (_f, writer) = setup();
        let fence = writer.reference_read_fence().unwrap();
        let (_other_f, other) = setup();
        let mut receiver = other.into_catalog_database();
        let error = receiver
            .readiness_store()
            .unwrap()
            .publish_streamed_feature_manifest(
                &fence,
                Path::new("one.wav"),
                50,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::sample_sources::readiness::ReadinessError::SourcePublicationFence(
                    crate::sample_sources::db::SourcePublicationFenceError::LeaseMismatch
                )
            ),
            "{error:?}"
        );
        let rows: i64 = receiver
            .connection
            .query_row("SELECT count(*) FROM source_readiness_targets", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }
    // These vectors qualify the actual owned SQL repair contract, not the DSP engine or disks.
    fn complete_feature(
        writer: &mut SourceWriter,
        target: &ReadinessTarget,
        now: i64,
    ) -> crate::sample_sources::readiness::FeatureArtifactKey {
        let revision = writer.current_revision().unwrap();
        writer
            .enqueue_readiness_jobs(
                &revision,
                &[ReadinessDeficit {
                    target: target.clone(),
                    reason: ReadinessClassification::Pending,
                    enqueued_at: None,
                }],
                now,
            )
            .unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, target, now, 100)
            .unwrap()
            .unwrap();
        match writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                now + 1,
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            FeatureArtifactPublication::Recorded(key) => key,
            other => panic!("expected completion: {other:?}"),
        }
    }
    fn feature_classification(
        writer: &SourceWriter,
        target: &ReadinessTarget,
        now: i64,
    ) -> ReadinessClassification {
        crate::sample_sources::readiness::ReadinessView::new(&writer.catalog_database().connection)
            .reconcile("manifest-features", now)
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.target.key() == target.key())
            .unwrap()
            .classification
    }
    #[test]
    fn feature_rebuild_missing_and_corrupt_done_payloads_are_not_current_and_requeue_exactly() {
        use crate::sample_sources::readiness::{
            FeatureArtifactFixtureFault as Fault, FeatureArtifactRebuildAdmission as Admission,
        };
        for (fault, removed) in [
            (Fault::MissingPayload, false),
            (Fault::CorruptPayload, true),
        ] {
            let (_f, mut writer) = setup();
            let target = publish(&mut writer);
            let old_key = complete_feature(&mut writer, &target, 31);
            let revision = writer.current_revision().unwrap();
            writer
                .inject_feature_artifact_fixture_fault(&revision, &target, fault)
                .unwrap();
            assert_eq!(
                feature_classification(&writer, &target, 33),
                ReadinessClassification::Pending
            );
            assert!(
                writer
                    .lookup_feature_artifact_for_target(&revision, &target)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                writer
                    .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                    .unwrap(),
                Admission::Admitted {
                    removed_corrupt_payload: removed
                }
            );
            assert_eq!(
                writer
                    .catalog_database()
                    .connection
                    .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                0
            );
            let new_key = complete_feature(&mut writer, &target, 34);
            assert_eq!(new_key, old_key);
            assert_eq!(
                feature_classification(&writer, &target, 35),
                ReadinessClassification::Current
            );
        }
    }
    #[test]
    fn feature_rebuild_missing_marker_preserves_healthy_bytes_and_reissues_completion() {
        use crate::sample_sources::readiness::{
            FeatureArtifactFixtureFault as Fault, FeatureArtifactRebuildAdmission as Admission,
        };
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        let key = complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        let before = writer
            .lookup_feature_artifact(&revision, &key)
            .unwrap()
            .unwrap();
        writer
            .inject_feature_artifact_fixture_fault(&revision, &target, Fault::MissingMarker)
            .unwrap();
        assert_eq!(
            feature_classification(&writer, &target, 33),
            ReadinessClassification::Pending
        );
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                .unwrap(),
            Admission::Admitted {
                removed_corrupt_payload: false
            }
        );
        assert_eq!(
            writer
                .lookup_feature_artifact(&revision, &key)
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(complete_feature(&mut writer, &target, 34), key);
        assert_eq!(
            writer
                .lookup_feature_artifact(&revision, &key)
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            feature_classification(&writer, &target, 35),
            ReadinessClassification::Current
        );
    }
    #[test]
    fn feature_rebuild_retains_unexpired_claim_and_retry_policy() {
        use crate::sample_sources::readiness::FeatureArtifactRebuildAdmission as Admission;
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        let revision = writer.current_revision().unwrap();
        writer
            .admit_feature_artifact_rebuild(&revision, &target, 31, &AtomicBool::new(false))
            .unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, &target, 32, 100)
            .unwrap()
            .unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                .unwrap(),
            Admission::Deferred
        );
        assert!(matches!(
            feature_classification(&writer, &target, 33),
            ReadinessClassification::Running { .. }
        ));
        writer
            .fail_readiness_work(
                &revision,
                claim.readiness_claim(),
                ReadinessFailureClassification::Retryable,
                "repair-test",
                "retain retry",
                33,
                ReadinessRetryPolicy::new(100, 1000, 4).unwrap(),
            )
            .unwrap();
        let before=writer.catalog_database().connection.query_row("SELECT status,retry_at,readiness_claim_generation FROM analysis_jobs WHERE readiness_stage='analysis_features'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,i64>(2)?))).unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 34, &AtomicBool::new(false))
                .unwrap(),
            Admission::Unchanged
        );
        let after=writer.catalog_database().connection.query_row("SELECT status,retry_at,readiness_claim_generation FROM analysis_jobs WHERE readiness_stage='analysis_features'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,i64>(2)?))).unwrap();
        assert_eq!(before, after);
    }
    #[test]
    fn feature_rebuild_ambiguous_reference_retains_payload_and_reports_ownership_failure() {
        use crate::sample_sources::readiness::FeatureArtifactRebuildAdmission as Admission;
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_feature_artifacts SET artifact_ref='foreign-reference'",
                [],
            )
            .unwrap();
        let revision = writer.current_revision().unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                .unwrap(),
            Admission::Ambiguous
        );
        assert!(matches!(
            feature_classification(&writer, &target, 33),
            ReadinessClassification::PermanentFailure { .. }
        ));
        assert_eq!(counts(&writer), (7, 3, 1));
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            1
        );
    }
    #[test]
    fn feature_rebuild_preserves_other_file_and_rejects_stale_target_or_cancel() {
        use crate::sample_sources::readiness::{
            FeatureArtifactFixtureFault as Fault, FeatureArtifactRebuildAdmission as Admission,
        };
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        let other = writer
            .prepare_feature_readiness_target(
                &revision,
                Path::new("two.wav"),
                33,
                &AtomicBool::new(false),
            )
            .unwrap();
        let other_key = complete_feature(&mut writer, &other, 34);
        let other_bytes = writer
            .lookup_feature_artifact(&revision, &other_key)
            .unwrap()
            .unwrap();
        writer
            .inject_feature_artifact_fixture_fault(&revision, &target, Fault::CorruptPayload)
            .unwrap();
        assert!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 36, &AtomicBool::new(true))
                .is_err()
        );
        let mut stale = target.clone();
        stale.content_generation = "changed-content".into();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &stale, 36, &AtomicBool::new(false))
                .unwrap(),
            Admission::RejectedStale
        );
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 36, &AtomicBool::new(false))
                .unwrap(),
            Admission::Admitted {
                removed_corrupt_payload: true
            }
        );
        assert_eq!(
            writer
                .lookup_feature_artifact(&revision, &other_key)
                .unwrap()
                .unwrap(),
            other_bytes
        );
        assert_eq!(
            feature_classification(&writer, &other, 36),
            ReadinessClassification::Current
        );
    }
    #[test]
    fn feature_rebuild_transaction_local_revision_change_rolls_back_corrupt_delete_and_queue() {
        use crate::sample_sources::readiness::FeatureArtifactFixtureFault as Fault;
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        writer
            .inject_feature_artifact_fixture_fault(&revision, &target, Fault::CorruptPayload)
            .unwrap();
        writer.catalog_database().connection.execute_batch("CREATE TRIGGER repair_changes_revision AFTER DELETE ON source_feature_artifacts BEGIN UPDATE metadata SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='revision'; END").unwrap();
        assert!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(writer.current_revision().unwrap(), revision);
        assert_eq!(counts(&writer), (7, 3, 1));
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row(
                    "SELECT status FROM analysis_jobs WHERE readiness_stage='analysis_features'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "done"
        );
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            1
        );
    }

    #[test]
    fn feature_rebuild_exhausted_claim_or_foreign_job_preserves_corrupt_evidence() {
        use crate::sample_sources::readiness::{
            FeatureArtifactFixtureFault as Fault, FeatureArtifactRebuildAdmission as Admission,
        };
        for mutation in [
            "UPDATE analysis_jobs SET readiness_claim_generation=4294967295 WHERE readiness_stage='analysis_features'",
            "UPDATE analysis_jobs SET artifact_version='foreign-features-v2' WHERE readiness_stage='analysis_features'",
        ] {
            let (_f, mut writer) = setup();
            let target = publish(&mut writer);
            complete_feature(&mut writer, &target, 31);
            let revision = writer.current_revision().unwrap();
            writer
                .inject_feature_artifact_fixture_fault(&revision, &target, Fault::CorruptPayload)
                .unwrap();
            writer
                .catalog_database()
                .connection
                .execute(mutation, [])
                .unwrap();
            assert_eq!(
                writer
                    .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                    .unwrap(),
                Admission::Ambiguous
            );
            assert_eq!(counts(&writer), (7, 3, 1));
            assert_eq!(
                writer
                    .catalog_database()
                    .connection
                    .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                1
            );
        }
    }
    fn audit_current_manifest(f: &CurationFixture, writer: &SourceWriter) {
        let db = writer.catalog_database();
        let revision = db.get_revision().unwrap();
        db.begin_source_directory_truth_generation(3, 0).unwrap();
        db.write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(
                3,
                revision,
                40,
                f.root_identity.clone(),
                None,
            )
            .unwrap();
    }
    #[test]
    fn feature_current_manifest_change_invalidates_old_ready_before_target_republication() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
        let old_key = complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        let other = writer
            .prepare_feature_readiness_target(
                &revision,
                Path::new("two.wav"),
                33,
                &AtomicBool::new(false),
            )
            .unwrap();
        let other_key = complete_feature(&mut writer, &other, 34);
        let other_payload = writer
            .lookup_feature_artifact(&revision, &other_key)
            .unwrap()
            .unwrap();
        let file = f._source.path().join("one.wav");
        std::fs::write(&file, b"new audited content").unwrap();
        let hash = blake3::hash(b"new audited content").to_hex().to_string();
        let mut batch = writer.catalog_database().write_batch().unwrap();
        batch
            .upsert_file_with_hash(Path::new("one.wav"), 19, 2, &hash)
            .unwrap();
        batch.commit().unwrap();
        audit_current_manifest(&f, &writer);
        let next_revision = writer.current_revision().unwrap();
        assert!(next_revision.revision() > revision.revision());
        assert_eq!(
            feature_classification(&writer, &target, 40),
            ReadinessClassification::StaleByGeneration
        );
        assert!(
            writer
                .lookup_feature_artifact(&next_revision, &old_key)
                .unwrap()
                .is_none()
        );
        assert!(
            writer
                .lookup_feature_artifact_for_target(&next_revision, &target)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            feature_classification(&writer, &other, 40),
            ReadinessClassification::Current
        );
        assert_eq!(
            writer
                .lookup_feature_artifact(&next_revision, &other_key)
                .unwrap()
                .unwrap(),
            other_payload
        );
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            2
        );
        let next = writer
            .prepare_feature_readiness_target(
                &next_revision,
                Path::new("one.wav"),
                41,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(next.scope_id, target.scope_id);
        assert_ne!(next.content_generation, target.content_generation);
        assert!(next.source_generation > target.source_generation);
        let key = complete_feature(&mut writer, &next, 42);
        assert_ne!(key, old_key);
        assert_eq!(
            feature_classification(&writer, &next, 43),
            ReadinessClassification::Current
        );
    }
    #[test]
    fn feature_current_manifest_rename_and_missing_retain_immutable_bytes_without_false_current() {
        for missing in [false, true] {
            let (f, mut writer) = setup();
            let target = publish(&mut writer);
            let key = complete_feature(&mut writer, &target, 31);
            let revision = writer.current_revision().unwrap();
            let payload = writer
                .lookup_feature_artifact(&revision, &key)
                .unwrap()
                .unwrap();
            let mut batch = writer.catalog_database().write_batch().unwrap();
            if missing {
                batch.set_missing(Path::new("one.wav"), true).unwrap();
            } else {
                std::fs::rename(
                    f._source.path().join("one.wav"),
                    f._source.path().join("renamed.wav"),
                )
                .unwrap();
                batch
                    .remap_wav_file_path(Path::new("one.wav"), Path::new("renamed.wav"))
                    .unwrap();
                // Remapping correctly clears old observations. Republish actual post-rename
                // identity/hash as the scanner does before declaring a complete audit.
                let file = f._source.path().join("renamed.wav");
                let metadata = file.metadata().unwrap();
                let identity =
                    crate::filesystem_identity::stable_filesystem_identity(&file, &metadata)
                        .unwrap();
                let hash = blake3::hash(&std::fs::read(&file).unwrap())
                    .to_hex()
                    .to_string();
                batch
                    .upsert_file_with_hash(Path::new("renamed.wav"), metadata.len(), 1, &hash)
                    .unwrap();
                batch
                    .set_file_identity(Path::new("renamed.wav"), Some(&identity))
                    .unwrap();
            }
            batch.commit().unwrap();
            audit_current_manifest(&f, &writer);
            let revision = writer.current_revision().unwrap();
            assert_eq!(
                feature_classification(&writer, &target, 40),
                ReadinessClassification::StaleByGeneration
            );
            assert!(
                writer
                    .lookup_feature_artifact(&revision, &key)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                writer
                    .catalog_database()
                    .connection
                    .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                1
            );
            if !missing {
                let next = writer
                    .prepare_feature_readiness_target(
                        &revision,
                        Path::new("renamed.wav"),
                        41,
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                assert_eq!(next.scope_id, target.scope_id);
                assert_eq!(next.content_generation, target.content_generation);
                assert_eq!(
                    writer
                        .lookup_feature_artifact(&revision, &key)
                        .unwrap()
                        .unwrap(),
                    payload
                );
                assert_eq!(
                    feature_classification(&writer, &next, 41),
                    ReadinessClassification::Current
                );
            }
        }
    }

    #[test]
    fn feature_current_manifest_change_during_publication_rolls_back_all_completion() {
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        let revision = writer.current_revision().unwrap();
        writer
            .admit_feature_artifact_rebuild(&revision, &target, 31, &AtomicBool::new(false))
            .unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, &target, 32, 100)
            .unwrap()
            .unwrap();
        writer.catalog_database().connection.execute_batch("CREATE TRIGGER completion_changes_manifest AFTER INSERT ON source_feature_artifacts BEGIN UPDATE wav_files SET content_hash='changed-inside-transaction' WHERE path='one.wav'; END").unwrap();
        let result = writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                33,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(result, FeatureArtifactPublication::RejectedStale);
        assert_eq!(counts(&writer), (7, 2, 1));
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row(
                    "SELECT content_hash FROM wav_files WHERE path='one.wav'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            target.content_generation
        );
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row(
                    "SELECT status FROM analysis_jobs WHERE readiness_stage='analysis_features'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "running"
        );
    }
}
