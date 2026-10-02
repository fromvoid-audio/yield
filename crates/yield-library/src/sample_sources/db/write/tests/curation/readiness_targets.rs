#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::sample_sources::SourceRevision;
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
            // Actual rename/missing owner fixture also runs the maintenance route. Missing
            // membership is retained evidence, while renamed identity/content remains current.
            if missing {
                writer
                    .prepare_feature_readiness_target(
                        &revision,
                        Path::new("two.wav"),
                        42,
                        &AtomicBool::new(false),
                    )
                    .unwrap();
            }
            assert_eq!(
                writer
                    .prune_superseded_feature_artifacts(
                        &revision,
                        None,
                        64,
                        &AtomicBool::new(false)
                    )
                    .unwrap()
                    .removed,
                0
            );
            assert_eq!(feature_owner_counts(&writer), (1, 1));
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
    #[test]
    fn feature_rebuild_metrics_partition_actual_owned_command_outcomes_and_committed_logical_bytes()
    {
        use crate::sample_sources::readiness::{
            FeatureArtifactFixtureFault as Fault, FeatureArtifactRebuildAdmission as Admission,
        };
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        let revision = writer.current_revision().unwrap();
        assert_eq!(
            writer.feature_rebuild_metrics(&revision).unwrap().attempted,
            0
        );
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 31, &AtomicBool::new(false))
                .unwrap(),
            Admission::Admitted {
                removed_corrupt_payload: false
            }
        );
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 31, &AtomicBool::new(false))
                .unwrap(),
            Admission::Unchanged
        );
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
        writer
            .publish_feature_artifact(
                &revision,
                &claim,
                &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                34,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 35, &AtomicBool::new(false))
                .unwrap(),
            Admission::Ready
        );
        writer
            .inject_feature_artifact_fixture_fault(&revision, &target, Fault::MissingMarker)
            .unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 36, &AtomicBool::new(false))
                .unwrap(),
            Admission::Admitted {
                removed_corrupt_payload: false
            }
        );
        let preserved = writer
            .catalog_database()
            .connection
            .query_row("SELECT payload FROM source_feature_artifacts", [], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap();
        complete_feature(&mut writer, &target, 37);
        assert_eq!(
            writer
                .catalog_database()
                .connection
                .query_row("SELECT payload FROM source_feature_artifacts", [], |r| r
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            preserved
        );
        writer
            .inject_feature_artifact_fixture_fault(&revision, &target, Fault::CorruptPayload)
            .unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 39, &AtomicBool::new(false))
                .unwrap(),
            Admission::Admitted {
                removed_corrupt_payload: true
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
        complete_feature(&mut writer, &target, 40);
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_feature_artifacts SET artifact_ref='foreign-reference'",
                [],
            )
            .unwrap();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 42, &AtomicBool::new(false))
                .unwrap(),
            Admission::Ambiguous
        );
        let mut stale = target.clone();
        stale.content_generation = "outdated-content".into();
        assert_eq!(
            writer
                .admit_feature_artifact_rebuild(&revision, &stale, 43, &AtomicBool::new(false))
                .unwrap(),
            Admission::RejectedStale
        );
        assert!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 44, &AtomicBool::new(true))
                .is_err()
        );
        let metrics = writer.feature_rebuild_metrics(&revision).unwrap();
        assert_eq!(metrics.attempted, 9);
        assert_eq!(
            (
                metrics.ready,
                metrics.admitted,
                metrics.unchanged,
                metrics.deferred,
                metrics.ambiguous,
                metrics.stale,
                metrics.cancelled,
                metrics.failed
            ),
            (1, 3, 1, 1, 1, 1, 1, 0)
        );
        assert_eq!(metrics.corrupt_payloads_removed, 1);
        assert_eq!(metrics.logical_payload_bytes_removed, 732);
        assert!(metrics.total_elapsed_us >= metrics.max_elapsed_us);
        assert!(!metrics.saturated);
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.len() < 768);
        assert!(!json.contains("manifest-features"));
        assert!(!json.contains("foreign-reference"));
    }
    #[test]
    fn feature_rebuild_metrics_reject_foreign_observer_and_reset_on_owner_reacquisition() {
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        let revision = writer.current_revision().unwrap();
        let (_other, foreign_writer) = setup();
        let foreign = foreign_writer.current_revision().unwrap();
        assert!(
            writer
                .admit_feature_artifact_rebuild(&foreign, &target, 31, &AtomicBool::new(false))
                .is_err()
        );
        assert!(writer.feature_rebuild_metrics(&foreign).is_err());
        let before = writer.feature_rebuild_metrics(&revision).unwrap();
        assert_eq!((before.attempted, before.stale), (1, 1));
        let db = writer.into_catalog_database();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 8).unwrap();
        let current = writer.current_revision().unwrap();
        assert_eq!(
            writer.feature_rebuild_metrics(&current).unwrap(),
            crate::sample_sources::FeatureRebuildMetrics::default()
        );
        assert!(writer.feature_rebuild_metrics(&revision).is_err());
        assert!(
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 32, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(
            (
                writer.feature_rebuild_metrics(&current).unwrap().attempted,
                writer.feature_rebuild_metrics(&current).unwrap().stale
            ),
            (1, 1)
        );
    }
    #[test]
    fn feature_rebuild_metrics_stale_revision_observation_and_command_reject_without_removal() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        let mut batch = writer.catalog_database().write_batch().unwrap();
        batch
            .upsert_file_with_hash(Path::new("one.wav"), 7, 2, &target.content_generation)
            .unwrap();
        batch.commit().unwrap();
        audit_current_manifest(&f, &writer);
        let current = writer.current_revision().unwrap();
        assert!(current.revision() > revision.revision());
        assert!(matches!(
            writer.admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false)),
            Err(SourceWriterError::StaleRevision { .. })
        ));
        assert!(writer.feature_rebuild_metrics(&revision).is_err());
        let metrics = writer.feature_rebuild_metrics(&current).unwrap();
        assert_eq!(
            (
                metrics.attempted,
                metrics.stale,
                metrics.logical_payload_bytes_removed
            ),
            (1, 1, 0)
        );
        assert_eq!(counts(&writer), (7, 3, 1));
    }
    #[test]
    fn feature_rebuild_unconfirmed_or_triggered_removal_rolls_back_and_never_counts_logical_bytes()
    {
        use crate::sample_sources::readiness::FeatureArtifactFixtureFault as Fault;
        for trigger in [
            "CREATE TRIGGER bad_removal BEFORE DELETE ON source_feature_artifacts BEGIN SELECT RAISE(IGNORE); END",
            "CREATE TRIGGER bad_removal AFTER DELETE ON source_feature_artifacts BEGIN INSERT INTO source_feature_artifacts VALUES(OLD.target_key,OLD.artifact_ref,OLD.payload,OLD.digest,OLD.claim_generation); END",
            "CREATE TRIGGER bad_removal AFTER DELETE ON source_feature_artifacts BEGIN DELETE FROM source_feature_artifacts WHERE target_key<>OLD.target_key; END",
            "CREATE TRIGGER bad_removal BEFORE DELETE ON source_feature_artifacts BEGIN SELECT RAISE(ABORT,'fixture rollback'); END",
        ] {
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
            let healthy = writer
                .lookup_feature_artifact(&revision, &other_key)
                .unwrap()
                .unwrap();
            writer
                .inject_feature_artifact_fixture_fault(&revision, &target, Fault::CorruptPayload)
                .unwrap();
            writer
                .catalog_database()
                .connection
                .execute_batch(trigger)
                .unwrap();
            assert!(
                writer
                    .admit_feature_artifact_rebuild(&revision, &target, 36, &AtomicBool::new(false))
                    .is_err()
            );
            let metrics = writer.feature_rebuild_metrics(&revision).unwrap();
            assert_eq!(
                (
                    metrics.attempted,
                    metrics.failed,
                    metrics.admitted,
                    metrics.logical_payload_bytes_removed
                ),
                (1, 1, 0, 0)
            );
            assert_eq!(
                writer
                    .lookup_feature_artifact(&revision, &other_key)
                    .unwrap()
                    .unwrap(),
                healthy
            );
            assert_eq!(counts(&writer), (7, 4, 1));
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
            assert_eq!(writer.catalog_database().connection.query_row("SELECT count(*) FROM analysis_jobs WHERE readiness_stage='analysis_features' AND status='done'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        }
    }
    fn assert_maintenance_metrics(
        writer: &SourceWriter,
        revision: &SourceRevision,
        expected: (u64, u64, u64, u64, u64, u64, u64, u64),
    ) {
        let m = writer.feature_maintenance_metrics(revision).unwrap();
        assert_eq!(
            (
                m.attempted,
                m.committed,
                m.stale,
                m.cancelled,
                m.failed,
                m.inspected_candidates,
                m.removed_rows,
                m.logical_payload_bytes_removed
            ),
            expected
        );
        assert_eq!(m.attempted, m.committed + m.stale + m.cancelled + m.failed);
        assert!(m.total_elapsed_us >= m.max_elapsed_us);
        assert!(!m.saturated);
    }
    fn feature_owner_counts(writer: &SourceWriter) -> (i64, i64) {
        writer.catalog_database().connection.query_row(
            "SELECT (SELECT count(*) FROM source_feature_artifacts),(SELECT count(*) FROM source_feature_reverse_owners)",
            [], |r| Ok((r.get(0)?,r.get(1)?)),
        ).unwrap()
    }
    fn supersede_feature(
        f: &CurationFixture,
        writer: &mut SourceWriter,
        target: &ReadinessTarget,
    ) -> ReadinessTarget {
        let bytes = b"actual superseding feature input";
        std::fs::write(f._source.path().join("one.wav"), bytes).unwrap();
        let hash = blake3::hash(bytes).to_hex().to_string();
        let mut batch = writer.catalog_database().write_batch().unwrap();
        batch
            .upsert_file_with_hash(Path::new("one.wav"), bytes.len() as u64, 3, &hash)
            .unwrap();
        batch
            .set_file_identity(Path::new("one.wav"), Some(&target.scope_id))
            .unwrap();
        batch.commit().unwrap();
        audit_current_manifest(f, writer);
        publish(writer)
    }
    #[test]
    fn feature_inverse_actual_publication_and_selective_prune_preserve_readiness_and_restart() {
        let (f, mut writer) = setup();
        let old = publish(&mut writer);
        let old_key = complete_feature(&mut writer, &old, 31);
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
        assert_eq!(feature_owner_counts(&writer), (2, 2));
        let unchanged = writer
            .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
            .unwrap();
        assert_eq!((unchanged.observed, unchanged.removed), (2, 0));
        assert_maintenance_metrics(&writer, &revision, (1, 1, 0, 0, 0, 2, 0, 0));
        let fresh = supersede_feature(&f, &mut writer, &old);
        let fresh_key = complete_feature(&mut writer, &fresh, 41);
        let revision = writer.current_revision().unwrap();
        let readiness_before = counts(&writer);
        let page = writer
            .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            (
                page.observed,
                page.removed,
                page.logical_payload_bytes_removed
            ),
            (3, 1, 732)
        );
        assert!(page.next.is_none());
        assert_maintenance_metrics(&writer, &revision, (2, 2, 0, 0, 0, 5, 1, 732));
        assert_eq!(counts(&writer), readiness_before);
        assert_eq!(feature_owner_counts(&writer), (2, 2));
        assert!(
            writer
                .lookup_feature_artifact(&revision, &fresh_key)
                .unwrap()
                .is_some()
        );
        assert!(
            writer
                .lookup_feature_artifact(&revision, &other_key)
                .unwrap()
                .is_some()
        );
        let absent: bool = writer
            .catalog_database()
            .connection
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM source_feature_artifacts WHERE artifact_ref=?1)",
                [old_key.reference()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(absent);
        drop(writer);
        let db = SourceDatabase::open_for_source_write_with_profile_guard(
            f._source.path(),
            &f.profile_guard,
            &f.profile_guard
                .profile_root()
                .join("curation-profile-bound"),
        )
        .unwrap();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 8).unwrap();
        assert_eq!(feature_owner_counts(&writer), (2, 2));
        let current = writer.current_revision().unwrap();
        assert!(
            writer
                .lookup_feature_artifact(&current, &fresh_key)
                .unwrap()
                .is_some()
        );
        assert!(
            writer
                .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
                .is_err()
        );
    }
    #[test]
    fn feature_inverse_legacy_adoption_is_exact_and_conflicting_descriptor_rolls_back() {
        for conflict in [false, true] {
            let (_f, mut writer) = setup();
            let target = publish(&mut writer);
            let key = complete_feature(&mut writer, &target, 31);
            let revision = writer.current_revision().unwrap();
            let c = &writer.catalog_database().connection;
            if conflict {
                c.execute(
                    "UPDATE source_feature_reverse_owners SET file_identity='foreign-identity'",
                    [],
                )
                .unwrap();
            } else {
                c.execute("DELETE FROM source_feature_reverse_owners", [])
                    .unwrap();
            }
            assert_eq!(
                writer
                    .prune_superseded_feature_artifacts(
                        &revision,
                        None,
                        64,
                        &AtomicBool::new(false)
                    )
                    .unwrap()
                    .removed,
                0
            );
            writer
                .inject_feature_artifact_fixture_fault(
                    &revision,
                    &target,
                    crate::sample_sources::readiness::FeatureArtifactFixtureFault::MissingMarker,
                )
                .unwrap();
            writer
                .admit_feature_artifact_rebuild(&revision, &target, 33, &AtomicBool::new(false))
                .unwrap();
            let claim = writer
                .claim_feature_artifact(&revision, &target, 34, 100)
                .unwrap()
                .unwrap();
            let result = writer.publish_feature_artifact(
                &revision,
                &claim,
                &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                35,
                &AtomicBool::new(false),
            );
            if conflict {
                assert!(result.is_err());
                assert!(
                    writer
                        .lookup_feature_artifact_for_target(&revision, &target)
                        .unwrap()
                        .is_none()
                );
            } else {
                assert!(matches!(
                    result.unwrap(),
                    FeatureArtifactPublication::Recorded(_)
                ));
                assert_eq!(feature_owner_counts(&writer), (1, 1));
                assert!(
                    writer
                        .lookup_feature_artifact(&revision, &key)
                        .unwrap()
                        .is_some()
                );
            }
        }
    }
    #[test]
    fn feature_inverse_cancellation_cursor_foreign_owner_and_stale_revision_preserve_payloads() {
        let (f, mut writer) = setup();
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
        complete_feature(&mut writer, &other, 34);
        for limit in [0, 65, u32::MAX] {
            assert!(
                writer
                    .prune_superseded_feature_artifacts(
                        &revision,
                        None,
                        limit,
                        &AtomicBool::new(false)
                    )
                    .is_err()
            );
        }
        assert!(
            writer
                .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(true))
                .is_err()
        );
        let first = writer
            .prune_superseded_feature_artifacts(&revision, None, 1, &AtomicBool::new(false))
            .unwrap();
        assert_eq!((first.observed, first.removed), (1, 0));
        let cursor = first.next.unwrap();
        let second = writer
            .prune_superseded_feature_artifacts(
                &revision,
                Some(&cursor),
                1,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!((second.observed, second.removed), (1, 0));
        assert!(second.next.is_none());
        let (_other_f, mut foreign) = setup();
        let foreign_revision = foreign.current_revision().unwrap();
        assert!(
            foreign
                .prune_superseded_feature_artifacts(
                    &foreign_revision,
                    Some(&cursor),
                    1,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        supersede_feature(&f, &mut writer, &target);
        let current = writer.current_revision().unwrap();
        assert!(
            writer
                .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            writer
                .prune_superseded_feature_artifacts(
                    &current,
                    Some(&cursor),
                    1,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(feature_owner_counts(&writer), (2, 2));
        assert_maintenance_metrics(&writer, &current, (8, 2, 2, 1, 3, 2, 0, 0));
        assert_maintenance_metrics(&foreign, &foreign_revision, (1, 0, 1, 0, 0, 0, 0, 0));
        assert!(writer.feature_maintenance_metrics(&revision).is_err());
    }
    #[test]
    fn feature_inverse_prune_retains_ambiguous_and_active_evidence_and_trigger_failure_rolls_back()
    {
        for fault in [
            "legacy",
            "wrong_descriptor",
            "bad_digest",
            "unknown_hash",
            "missing",
            "current_alias",
            "unknown_control",
            "foreign_control",
            "running",
            "offline",
            "trigger",
        ] {
            let (f, mut writer) = setup();
            let target = publish(&mut writer);
            complete_feature(&mut writer, &target, 31);
            supersede_feature(&f, &mut writer, &target);
            let revision = writer.current_revision().unwrap();
            let connection = &writer.catalog_database().connection;
            match fault {
                "legacy" => {
                    connection
                        .execute("DELETE FROM source_feature_reverse_owners", [])
                        .unwrap();
                }
                "wrong_descriptor" => {
                    connection
                        .execute(
                            "UPDATE source_feature_reverse_owners SET content_generation=?1",
                            ["a".repeat(64)],
                        )
                        .unwrap();
                }
                "bad_digest" => {
                    connection
                        .execute(
                            "UPDATE source_feature_artifacts SET digest=zeroblob(32)",
                            [],
                        )
                        .unwrap();
                }
                "unknown_hash" => {
                    connection
                        .execute(
                            "UPDATE wav_files SET content_hash=NULL WHERE path='one.wav'",
                            [],
                        )
                        .unwrap();
                }
                "missing" => {
                    connection
                        .execute("UPDATE wav_files SET missing=1 WHERE path='one.wav'", [])
                        .unwrap();
                }
                "current_alias" => {
                    // Retained SQL observations may contain an old alias while a different
                    // path has newer content. Neither name supplies an absence proof.
                    connection.execute("UPDATE wav_files SET file_identity=?1,content_hash=?2 WHERE path='two.wav'",rusqlite::params![target.scope_id,target.content_generation]).unwrap();
                }
                "running" | "unknown_control" | "foreign_control" => {
                    connection.execute("INSERT INTO analysis_jobs(sample_id,source_id,job_type,status,created_at,readiness_managed,readiness_scope_kind,readiness_scope_id,readiness_stage,artifact_version,source_generation,content_generation,lease_expires_at,readiness_claim_generation) VALUES (?1,?2,'readiness_analysis_features_v1','running',30,1,'file',?1,'analysis_features',?3,?4,?5,9999,1)",rusqlite::params![target.scope_id,target.source_id,target.required_version,target.source_generation,target.content_generation]).unwrap();
                    if fault == "unknown_control" {
                        connection
                            .execute(
                                "UPDATE analysis_jobs SET artifact_version=NULL,status='done'",
                                [],
                            )
                            .unwrap();
                    }
                    if fault == "foreign_control" {
                        connection
                            .execute(
                                "UPDATE analysis_jobs SET source_id='foreign-source',status='done'",
                                [],
                            )
                            .unwrap();
                    }
                }
                "offline" => {
                    connection
                        .execute(
                            "UPDATE source_readiness_sources SET availability='offline'",
                            [],
                        )
                        .unwrap();
                }
                "trigger" => {
                    connection.execute_batch("CREATE TRIGGER refuse_feature_prune BEFORE DELETE ON source_feature_reverse_owners BEGIN SELECT RAISE(ABORT,'retained inverse'); END;").unwrap();
                }
                _ => unreachable!(),
            }
            let before = feature_owner_counts(&writer);
            let result = writer.prune_superseded_feature_artifacts(
                &revision,
                None,
                64,
                &AtomicBool::new(false),
            );
            if matches!(fault, "offline" | "trigger") {
                assert!(result.is_err(), "{fault}");
            } else {
                assert_eq!(result.unwrap().removed, 0, "{fault}");
            }
            assert_eq!(feature_owner_counts(&writer), before, "{fault}");
            if matches!(fault, "offline" | "trigger") {
                assert_maintenance_metrics(&writer, &revision, (1, 0, 0, 0, 1, 0, 0, 0));
            } else {
                assert_maintenance_metrics(&writer, &revision, (1, 1, 0, 0, 0, 1, 0, 0));
            }
        }
    }
    #[test]
    fn feature_inverse_publication_capacity_and_trigger_rejection_roll_back_completion() {
        for full in [false, true] {
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
            if full {
                writer.catalog_database().connection.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO source_feature_reverse_owners SELECT printf('%064d',i),'legacy-source',printf('legacy-file-%d',i),printf('%064d',i),'features-v1' FROM n;").unwrap();
            } else {
                writer.catalog_database().connection.execute_batch("CREATE TRIGGER refuse_feature_inverse BEFORE INSERT ON source_feature_reverse_owners BEGIN SELECT RAISE(ABORT,'retain publication'); END;").unwrap();
            }
            let before = counts(&writer);
            let result = writer.publish_feature_artifact(
                &revision,
                &claim,
                &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                33,
                &AtomicBool::new(false),
            );
            assert!(result.is_err());
            assert_eq!(counts(&writer), before);
            assert_eq!(
                feature_owner_counts(&writer),
                (0, if full { 4096 } else { 0 })
            );
            assert!(
                writer
                    .lookup_feature_artifact_for_target(&revision, &target)
                    .unwrap()
                    .is_none()
            );
            if full {
                writer
                    .catalog_database()
                    .connection
                    .execute("DELETE FROM source_feature_reverse_owners", [])
                    .unwrap();
            } else {
                writer
                    .catalog_database()
                    .connection
                    .execute_batch("DROP TRIGGER refuse_feature_inverse")
                    .unwrap();
            }
            assert!(matches!(
                writer
                    .publish_feature_artifact(
                        &revision,
                        &claim,
                        &FeatureArtifactPayload::from_v1_vector(&[0.5; 183]).unwrap(),
                        34,
                        &AtomicBool::new(false)
                    )
                    .unwrap(),
                FeatureArtifactPublication::Recorded(_)
            ));
            assert_eq!(feature_owner_counts(&writer), (1, 1));
        }
    }
    #[test]
    fn feature_inverse_candidate_pages_and_alias_evidence_are_bounded() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        // Ambiguous legacy payloads are real retained rows, not pruning authorization.
        writer.catalog_database().connection.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<64) INSERT INTO source_feature_artifacts SELECT printf('%064x',i),printf('legacy:%d',i),zeroblob(732),zeroblob(32),1 FROM n;").unwrap();
        let first = writer
            .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
            .unwrap();
        assert_eq!((first.observed, first.removed), (64, 0));
        let second = writer
            .prune_superseded_feature_artifacts(
                &revision,
                first.next.as_ref(),
                64,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!((second.observed, second.removed), (1, 0));
        assert!(second.next.is_none());
        let fresh = supersede_feature(&f, &mut writer, &target);
        let current = writer.current_revision().unwrap();
        writer.catalog_database().connection.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<65) INSERT INTO wav_files(path,file_size,modified_ns,file_identity,content_hash) SELECT printf('retained-alias-%d.wav',i),1,1,?1,?2 FROM n",rusqlite::params![target.scope_id,fresh.content_generation]).unwrap();
        let first = writer
            .prune_superseded_feature_artifacts(&current, None, 64, &AtomicBool::new(false))
            .unwrap();
        let second = writer
            .prune_superseded_feature_artifacts(
                &current,
                first.next.as_ref(),
                64,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(first.removed + second.removed, 0);
        assert_eq!(feature_owner_counts(&writer), (65, 1));
    }
    #[test]
    fn feature_inverse_cancellation_and_elapsed_sql_work_roll_back_after_delete_and_retry() {
        for elapsed in [false, true] {
            let (f, mut writer) = setup();
            let target = publish(&mut writer);
            complete_feature(&mut writer, &target, 31);
            supersede_feature(&f, &mut writer, &target);
            let current = writer.current_revision().unwrap();
            let cancel = std::sync::Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            // Test-only fault in the actual retained SQL command, not a clock/grant input.
            writer.catalog_database().connection.authorizer(Some(
                move |context: rusqlite::hooks::AuthContext<'_>| {
                    if matches!(
                        context.action,
                        rusqlite::hooks::AuthAction::Delete {
                            table_name: "source_feature_artifacts"
                        }
                    ) {
                        if elapsed {
                            std::thread::sleep(std::time::Duration::from_millis(2100));
                        } else {
                            flag.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    rusqlite::hooks::Authorization::Allow
                },
            ));
            let result = writer.prune_superseded_feature_artifacts(&current, None, 64, &cancel);
            assert!(matches!(
                result,
                Err(SourceWriterError::Readiness(
                    crate::sample_sources::readiness::ReadinessError::Cancelled
                        | crate::sample_sources::readiness::ReadinessError::AdmissionBudget(_)
                ))
            ));
            assert_eq!(feature_owner_counts(&writer), (1, 1));
            assert_maintenance_metrics(
                &writer,
                &current,
                (1, 0, 0, u64::from(!elapsed), u64::from(elapsed), 0, 0, 0),
            );
            writer.catalog_database().connection.authorizer(
                None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
            );
            cancel.store(false, std::sync::atomic::Ordering::Release);
            assert_eq!(
                writer
                    .prune_superseded_feature_artifacts(&current, None, 64, &cancel)
                    .unwrap()
                    .removed,
                1
            );
            assert_maintenance_metrics(
                &writer,
                &current,
                (2, 1, 0, u64::from(!elapsed), u64::from(elapsed), 1, 1, 732),
            );
        }
    }
    #[test]
    fn feature_inverse_cancellation_during_final_audit_rolls_back_before_commit() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        supersede_feature(&f, &mut writer, &target);
        let current = writer.current_revision().unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let deleting = std::sync::Arc::new(AtomicBool::new(false));
        let phase = deleting.clone();
        writer.catalog_database().connection.authorizer(Some(
            move |context: rusqlite::hooks::AuthContext<'_>| {
                match context.action {
                    rusqlite::hooks::AuthAction::Delete {
                        table_name: "source_feature_artifacts",
                    } => phase.store(true, std::sync::atomic::Ordering::Release),
                    rusqlite::hooks::AuthAction::Read {
                        table_name: "metadata",
                        ..
                    } if phase.load(std::sync::atomic::Ordering::Acquire) => {
                        flag.store(true, std::sync::atomic::Ordering::Release)
                    }
                    _ => {}
                }
                rusqlite::hooks::Authorization::Allow
            },
        ));
        let result = writer.prune_superseded_feature_artifacts(&current, None, 64, &cancel);
        assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(matches!(
            result,
            Err(SourceWriterError::Readiness(
                crate::sample_sources::readiness::ReadinessError::Cancelled
            ))
        ));
        assert_eq!(feature_owner_counts(&writer), (1, 1));
        assert_maintenance_metrics(&writer, &current, (1, 0, 0, 1, 0, 0, 0, 0));
        writer.catalog_database().connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        cancel.store(false, std::sync::atomic::Ordering::Release);
        assert_eq!(
            writer
                .prune_superseded_feature_artifacts(&current, None, 64, &cancel)
                .unwrap()
                .removed,
            1
        );
        assert_maintenance_metrics(&writer, &current, (2, 1, 0, 1, 0, 1, 1, 732));
    }
    #[test]
    fn feature_inverse_retained_hard_wal_blocks_delete_but_allows_readonly_retention_then_retry() {
        let (f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let current = writer.current_revision().unwrap();
        let reader = rusqlite::Connection::open_with_flags(
            &writer.catalog_database().db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM source_feature_artifacts")
            .unwrap();
        writer
            .catalog_database()
            .connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES ('feature-prune-pressure',zeroblob(?1))",
                [crate::WAL_HARD_WATERMARK_BYTES as i64],
            )
            .unwrap();
        assert_eq!(
            writer
                .prune_superseded_feature_artifacts(&current, None, 64, &AtomicBool::new(false))
                .unwrap()
                .removed,
            0
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        writer
            .catalog_database()
            .connection
            .execute(
                "DELETE FROM metadata WHERE key='feature-prune-pressure'",
                [],
            )
            .unwrap();
        writer
            .catalog_database()
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        supersede_feature(&f, &mut writer, &target);
        let current = writer.current_revision().unwrap();
        let reader = rusqlite::Connection::open_with_flags(
            &writer.catalog_database().db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM source_feature_artifacts")
            .unwrap();
        writer
            .catalog_database()
            .connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES ('feature-prune-pressure',zeroblob(?1))",
                [crate::WAL_HARD_WATERMARK_BYTES as i64],
            )
            .unwrap();
        let before = counts(&writer);
        assert!(matches!(
            writer.prune_superseded_feature_artifacts(&current, None, 64, &AtomicBool::new(false)),
            Err(SourceWriterError::Readiness(
                crate::sample_sources::readiness::ReadinessError::WalAdmission(
                    crate::WalAdmissionError::HardWatermark { .. }
                )
            ))
        ));
        assert_eq!(feature_owner_counts(&writer), (1, 1));
        assert_eq!(counts(&writer), before);
        assert_maintenance_metrics(&writer, &current, (2, 1, 0, 0, 1, 1, 0, 0));
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        writer
            .catalog_database()
            .connection
            .execute(
                "DELETE FROM metadata WHERE key='feature-prune-pressure'",
                [],
            )
            .unwrap();
        writer
            .catalog_database()
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        assert_eq!(
            writer
                .prune_superseded_feature_artifacts(&current, None, 64, &AtomicBool::new(false))
                .unwrap()
                .removed,
            1
        );
        assert_maintenance_metrics(&writer, &current, (3, 2, 0, 0, 1, 2, 1, 732));
    }
    #[test]
    fn feature_maintenance_metrics_observer_is_readonly_and_resets_on_handoff() {
        let (_f, mut writer) = setup();
        let target = publish(&mut writer);
        complete_feature(&mut writer, &target, 31);
        let revision = writer.current_revision().unwrap();
        let (_other_f, foreign) = setup();
        let foreign_revision = foreign.current_revision().unwrap();
        assert!(
            writer
                .prune_superseded_feature_artifacts(
                    &foreign_revision,
                    None,
                    64,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert!(
            writer
                .feature_maintenance_metrics(&foreign_revision)
                .is_err()
        );
        writer
            .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
            .unwrap();
        assert_maintenance_metrics(&writer, &revision, (2, 1, 1, 0, 0, 1, 0, 0));
        let db = writer.into_catalog_database();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("manifest-features"), 8).unwrap();
        let current = writer.current_revision().unwrap();
        assert_eq!(
            writer.feature_maintenance_metrics(&current).unwrap(),
            crate::sample_sources::FeatureMaintenanceMetrics::default()
        );
        assert!(writer.feature_maintenance_metrics(&revision).is_err());
        assert!(
            writer
                .prune_superseded_feature_artifacts(&revision, None, 64, &AtomicBool::new(false))
                .is_err()
        );
        assert_maintenance_metrics(&writer, &current, (1, 0, 1, 0, 0, 0, 0, 0));

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
                [now + 30],
            )
            .unwrap();
        let persisted = |writer: &SourceWriter| {
            let file = PathBuf::from(writer.catalog_database().connection.path().unwrap());
            [
                file.clone(),
                PathBuf::from(format!("{}-wal", file.display())),
            ]
            .into_iter()
            .filter_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
            .collect::<Vec<_>>()
        };
        let before = persisted(&writer);
        let changes = writer.catalog_database().connection.total_changes();
        writer
            .catalog_database()
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        assert_maintenance_metrics(&writer, &current, (1, 0, 1, 0, 0, 0, 0, 0));
        assert_maintenance_metrics(&writer, &current, (1, 0, 1, 0, 0, 0, 0, 0));
        assert_eq!(
            writer.catalog_database().connection.total_changes(),
            changes
        );
        assert_eq!(
            persisted(&writer),
            before,
            "observer renewed lease or wrote persistent bytes"
        );
        writer
            .catalog_database()
            .connection
            .pragma_update(None, "query_only", false)
            .unwrap();
        writer
            .catalog_database()
            .connection
            .execute(
                "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
                [],
            )
            .unwrap();
        let before = persisted(&writer);
        let changes = writer.catalog_database().connection.total_changes();
        assert!(writer.feature_maintenance_metrics(&current).is_err());
        assert!(
            writer
                .prune_superseded_feature_artifacts(&current, None, 64, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(
            writer.catalog_database().connection.total_changes(),
            changes
        );
        assert_eq!(
            persisted(&writer),
            before,
            "expired observation or command renewed lease"
        );
        // Explicit mutation admission is the only lease renewal here; then expose the error count.
        let renewed = writer.current_revision_for_command().unwrap();
        assert_maintenance_metrics(&writer, &renewed, (2, 0, 1, 0, 1, 0, 0, 0));
        assert_eq!(feature_owner_counts(&writer), (1, 1));
    }
}
