#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::{FeatureJobFailure, FeatureJobOutcome, prepare_feature_job};
    use crate::{TransientJobError, prepare_transient_job};
    use yield_analysis::transients::TransientPreset;
    use yield_library::sample_sources::db::transient_cache::CachedTransientMarkers;
    use yield_library::sample_sources::readiness::{
        FeatureArtifactPublication, ReadinessClassification, ReadinessDeficit, ReadinessTarget,
        SourceFeatureInputError,
    };
    use yield_library::sample_sources::{SourceDatabaseConnectionRole, SourceWriter};

    fn setup() -> (Fixture, SourceWriter, ReadinessTarget) {
        setup_relative(Path::new("tone.wav"))
    }

    fn setup_relative(relative: &Path) -> (Fixture, SourceWriter, ReadinessTarget) {
        setup_signal(relative, |frame| {
            ((frame as f32 * 0.1).sin() * 12_000.0) as i16
        })
    }

    fn setup_signal(
        relative: &Path,
        sample: impl Fn(usize) -> i16,
    ) -> (Fixture, SourceWriter, ReadinessTarget) {
        let fixture = Fixture::new("feature-job-tests");
        let file = fixture.root.path().join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut wav = hound::WavWriter::create(
            &file,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..16_000 {
            wav.write_sample(sample(frame)).unwrap();
        }
        wav.finalize().unwrap();
        let bytes = fs::read(&file).unwrap();
        let content = blake3::hash(&bytes).to_hex().to_string();
        let meta = file.metadata().unwrap();
        let identity = stable_filesystem_identity(&file, &meta).unwrap();
        let modified = i64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            Path::new("feature-job-db"),
        )
        .unwrap();
        let writer =
            SourceWriter::new_for_scan(db, SourceId::from_string("feature-job-source"), 1).unwrap();
        let root = writer.current_scan_fence().unwrap().root_identity().clone();
        let db = writer.into_catalog_database();
        let mut batch = db.write_batch().unwrap();
        batch
            .upsert_file_with_hash(relative, meta.len(), modified, &content)
            .unwrap();
        batch.set_file_identity(relative, Some(&identity)).unwrap();
        batch.commit().unwrap();
        let revision = db.get_revision().unwrap();
        db.begin_source_directory_truth_generation(1, 0).unwrap();
        db.write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(1, revision, 1, root, None)
            .unwrap();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("feature-job-source"), 1).unwrap();
        let revision = writer.current_revision().unwrap();
        let target = writer
            .prepare_feature_readiness_target(&revision, relative, 1, &AtomicBool::new(false))
            .unwrap();
        writer
            .enqueue_readiness_jobs(
                &revision,
                &[ReadinessDeficit {
                    target: target.clone(),
                    reason: ReadinessClassification::Pending,
                    enqueued_at: None,
                }],
                10,
            )
            .unwrap();
        (fixture, writer, target)
    }

    fn observer(fixture: &Fixture) -> rusqlite::Connection {
        SourceDatabase::open_connection_with_role_and_database_root(
            fixture.root.path(),
            fixture.guard.profile_root().join("feature-job-db"),
            SourceDatabaseConnectionRole::UiRead,
        )
        .unwrap()
    }
    fn status(fixture: &Fixture) -> String {
        observer(fixture)
            .query_row(
                "SELECT status FROM analysis_jobs WHERE readiness_stage='analysis_features'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn artifact_count(fixture: &Fixture) -> i64 {
        observer(fixture)
            .query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn transient_cache_reopens_by_exact_config_and_refuses_a_stale_input() {
        let (fixture, writer, _) = setup();
        let relative = RootRelativePath::try_from_path("tone.wav".into()).unwrap();
        let media = fixture.root.path().join(relative.as_path());
        let original = fs::read(&media).unwrap();
        let current = writer.current_revision().unwrap();
        let held = writer.prepare_audio_input(&current, &relative).unwrap();
        let markers = CachedTransientMarkers::new(16_000, vec![1237, 9673]).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(held.read_verified_bytes(&cancel).unwrap(), original);
        assert!(
            writer
                .lookup_transient_cache(&held, "detector-v1", "normal")
                .unwrap()
                .is_none()
        );
        assert!(
            writer
                .store_transient_cache(&held, "detector-v1", "normal", &markers, &cancel)
                .unwrap()
        );
        assert!(
            !writer
                .store_transient_cache(
                    &held,
                    "detector-v1",
                    "low",
                    &markers,
                    &AtomicBool::new(true)
                )
                .unwrap()
        );
        drop(held);
        drop(writer);

        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            Path::new("feature-job-db"),
        )
        .unwrap();
        let writer =
            SourceWriter::new(database, SourceId::from_string("feature-job-source"), 2).unwrap();
        let revision = writer.current_revision().unwrap();
        let held = writer.prepare_audio_input(&revision, &relative).unwrap();
        assert_eq!(
            writer
                .lookup_transient_cache(&held, "detector-v1", "normal")
                .unwrap(),
            Some(markers)
        );
        for (version, preset) in [("detector-v2", "normal"), ("detector-v1", "low")] {
            assert!(
                writer
                    .lookup_transient_cache(&held, version, preset)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(fs::read(&media).unwrap(), original);
        assert_eq!(status(&fixture), "pending");
        let displaced = media.with_extension("old");
        fs::rename(&media, &displaced).unwrap();
        fs::copy(&displaced, &media).unwrap();
        assert!(
            writer
                .lookup_transient_cache(&held, "detector-v1", "normal")
                .is_err()
        );
    }

    #[test]
    fn transient_job_returns_exact_markers_from_an_audited_wav() {
        let relative = Path::new("nested/attacks.wav");
        let (fixture, writer, _) = setup_signal(relative, |frame| match frame {
            1237 => 24_000,
            9673 => -24_000,
            _ => 0,
        });
        let revision = writer.current_revision().unwrap();
        let path = RootRelativePath::try_from_path(relative.into()).unwrap();
        let job = prepare_transient_job(
            &writer,
            &revision,
            &path,
            TransientPreset::Normal,
            FeatureAnalysisCancellation::new(),
        )
        .unwrap();
        let computed = thread::spawn(move || job.compute()).join().unwrap();
        let markers = computed.finish(&writer).unwrap();
        assert_eq!(markers.frames(), [1237, 9673]);
        assert_eq!(markers.source_frames(), 16_000);
        assert_eq!(markers.preset(), TransientPreset::Normal);
        assert_eq!(status(&fixture), "pending");
        assert_eq!(artifact_count(&fixture), 0);
    }

    #[test]
    fn transient_job_rejects_cancellation_and_file_replacement_after_compute() {
        let (fixture, writer, _) = setup();
        let revision = writer.current_revision().unwrap();
        let path = RootRelativePath::try_from_path("tone.wav".into()).unwrap();
        let compute = |cancellation| {
            prepare_transient_job(
                &writer,
                &revision,
                &path,
                TransientPreset::High,
                cancellation,
            )
            .unwrap()
            .compute()
        };
        let cancellation = FeatureAnalysisCancellation::new();
        let computed = compute(cancellation.clone());
        cancellation.cancel();
        assert!(matches!(
            computed.finish(&writer),
            Err(TransientJobError::Cancelled)
        ));

        let computed = compute(FeatureAnalysisCancellation::new());
        let file = fixture.root.path().join(path.as_path());
        let displaced = file.with_extension("old");
        fs::rename(&file, &displaced).unwrap();
        fs::copy(&displaced, &file).unwrap();
        assert!(matches!(
            computed.finish(&writer),
            Err(TransientJobError::Source(SourceWriterError::FeatureInput(
                SourceFeatureInputError::Changed
            )))
        ));
        assert_eq!(status(&fixture), "pending");
        assert_eq!(artifact_count(&fixture), 0);
    }

    #[test]
    fn audited_audio_input_reads_exact_bytes_without_claiming_a_job() {
        let relative = Path::new("nested/tone.wav");
        let (fixture, writer, _) = setup_relative(relative);
        let revision = writer.current_revision().unwrap();
        let path = RootRelativePath::try_from_path(relative.into()).unwrap();
        let held = writer.prepare_audio_input(&revision, &path).unwrap();
        let expected = fs::read(fixture.root.path().join(relative)).unwrap();
        assert_eq!(
            held.read_verified_bytes(&AtomicBool::new(false)).unwrap(),
            expected,
        );
        writer.validate_audio_input(&held).unwrap();
        assert_eq!(writer.current_revision().unwrap(), revision);
        assert_eq!(status(&fixture), "pending");
        assert_eq!(artifact_count(&fixture), 0);

        drop(writer);
        assert_eq!(held.validate(), Err(SourceFeatureInputError::Changed));
    }

    #[test]
    fn audited_audio_input_rejects_changed_manifest_before_publication() {
        let (_fixture, writer, _) = setup();
        let path = RootRelativePath::try_from_path("tone.wav".into()).unwrap();
        let revision = writer.current_revision().unwrap();
        let held = writer.prepare_audio_input(&revision, &path).unwrap();
        let fence = writer.current_scan_fence().unwrap();
        let mut batch = writer.catalog_database().write_batch().unwrap();
        batch.set_missing(path.as_path(), true).unwrap();
        writer.commit_scan_manifest_batch(&fence, batch).unwrap();

        assert!(writer.prepare_audio_input(&revision, &path).is_err());
        assert!(writer.validate_audio_input(&held).is_err());
    }

    #[test]
    fn audited_audio_input_rejects_replaced_file_before_publication() {
        let (fixture, writer, _) = setup();
        let path = RootRelativePath::try_from_path("tone.wav".into()).unwrap();
        let revision = writer.current_revision().unwrap();
        let held = writer.prepare_audio_input(&revision, &path).unwrap();
        let file = fixture.root.path().join(path.as_path());
        let displaced = file.with_extension("old");
        fs::rename(&file, &displaced).unwrap();
        fs::copy(&displaced, &file).unwrap();

        assert!(matches!(
            writer.validate_audio_input(&held),
            Err(SourceWriterError::FeatureInput(
                SourceFeatureInputError::Changed
            ))
        ));
        assert!(writer.prepare_audio_input(&revision, &path).is_err());
        assert_eq!(status(&fixture), "pending");
    }

    #[test]
    fn feature_job_computes_real_vector_on_thread_before_atomic_ready() {
        let (fixture, mut writer, target) = setup();
        assert_eq!(status(&fixture), "pending");
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        assert_eq!(status(&fixture), "running");
        assert_eq!(artifact_count(&fixture), 0);
        let computed = thread::spawn(move || job.compute(&AtomicBool::new(false)))
            .join()
            .unwrap();
        assert_eq!(status(&fixture), "running");
        assert_eq!(artifact_count(&fixture), 0);
        let FeatureJobOutcome::Published(FeatureArtifactPublication::Recorded(key)) = computed
            .finish(&mut writer, 11, &AtomicBool::new(false))
            .unwrap()
        else {
            panic!("published")
        };
        assert_eq!(status(&fixture), "done");
        assert_eq!(artifact_count(&fixture), 1);
        let revision = writer.current_revision().unwrap();
        let actual = writer
            .lookup_feature_artifact(&revision, &key)
            .unwrap()
            .unwrap();
        let expected = yield_analysis::compute_feature_vector_v1_for_path(
            &fixture.root.path().join("tone.wav"),
        )
        .unwrap();
        let expected_bytes =
            yield_library::sample_sources::readiness::FeatureArtifactPayload::from_v1_vector(
                &expected,
            )
            .unwrap();
        assert_eq!(actual.bytes(), expected_bytes.bytes());
    }

    #[test]
    fn feature_job_changed_bytes_never_mark_ready_and_retain_retry() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        fs::write(fixture.root.path().join("tone.wav"), b"changed").unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        assert!(matches!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Failed(FeatureJobFailure::Input(_))
        ));
        assert_eq!(status(&fixture), "failed");
        assert_eq!(artifact_count(&fixture), 0);
        let (kind, retry): (String, i64) = observer(&fixture).query_row("SELECT failure_kind, retry_at FROM analysis_jobs WHERE readiness_stage='analysis_features'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(kind, "retryable");
        assert!(retry > 11);
    }

    #[test]
    fn feature_job_cancellation_preserves_pending_claim_without_artifact() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let computed = job.compute(&AtomicBool::new(true));
        assert!(matches!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Cancelled
        ));
        assert_eq!(status(&fixture), "pending");
        assert_eq!(artifact_count(&fixture), 0);
    }

    #[test]
    fn feature_job_restarted_owner_cannot_publish_old_computation() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        let db = writer.into_catalog_database();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("feature-job-source"), 2).unwrap();
        assert!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(status(&fixture), "running");
        assert_eq!(artifact_count(&fixture), 0);
        // Durable expired claim can be recovered under the current owner, never silently discarded.
        assert!(
            prepare_feature_job(&mut writer, &target, 111, 100)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn feature_job_source_root_replacement_cannot_publish_computed_result() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        let root = fixture.root.path();
        let displaced = root.with_extension("displaced");
        fs::rename(root, &displaced).unwrap();
        fs::create_dir(root).unwrap();
        let result = computed.finish(&mut writer, 11, &AtomicBool::new(false));
        fs::remove_dir(root).unwrap();
        fs::rename(&displaced, root).unwrap();
        assert!(result.is_err());
        assert_eq!(artifact_count(&fixture), 0);
        assert_eq!(status(&fixture), "running");
    }

    #[test]
    fn feature_job_symlink_replacement_preserves_external_target() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.wav"), b"external").unwrap();
        fs::remove_file(fixture.root.path().join("tone.wav")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("outside.wav"),
            fixture.root.path().join("tone.wav"),
        )
        .unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(
            outside.path().join("outside.wav"),
            fixture.root.path().join("tone.wav"),
        )
        .unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        assert!(matches!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Failed(FeatureJobFailure::Input(SourceFeatureInputError::Changed))
        ));
        assert_eq!(
            fs::read(outside.path().join("outside.wav")).unwrap(),
            b"external"
        );
        assert_eq!(artifact_count(&fixture), 0);
    }

    #[test]
    fn feature_job_expired_result_is_rejected_without_ready_or_false_cancel() {
        let (fixture, mut writer, target) = setup();
        let job = prepare_feature_job(&mut writer, &target, 10, 1)
            .unwrap()
            .unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        assert!(matches!(
            computed
                .finish(&mut writer, 12, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::RejectedStale
        ));
        assert_eq!(artifact_count(&fixture), 0);
        let next = prepare_feature_job(&mut writer, &target, 12, 1)
            .unwrap()
            .unwrap();
        let computed = next.compute(&AtomicBool::new(true));
        assert!(matches!(
            computed
                .finish(&mut writer, 14, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::RejectedStale
        ));
        assert_eq!(status(&fixture), "running");
    }

    #[test]
    fn feature_job_reclaimed_generation_rejects_old_result_and_publishes_current() {
        let (fixture, mut writer, target) = setup();
        let old = prepare_feature_job(&mut writer, &target, 10, 1)
            .unwrap()
            .unwrap()
            .compute(&AtomicBool::new(false));
        let current = prepare_feature_job(&mut writer, &target, 12, 100)
            .unwrap()
            .unwrap()
            .compute(&AtomicBool::new(false));
        assert!(matches!(
            old.finish(&mut writer, 13, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::RejectedStale
        ));
        assert_eq!(status(&fixture), "running");
        assert_eq!(artifact_count(&fixture), 0);
        assert!(matches!(
            current
                .finish(&mut writer, 14, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Published(_)
        ));
        assert_eq!(status(&fixture), "done");
        assert_eq!(artifact_count(&fixture), 1);
    }

    #[test]
    fn feature_job_held_input_permits_are_bounded_and_refunded() {
        let (fixture, mut writer, target) = setup();
        let revision = writer.current_revision().unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, &target, 10, 100)
            .unwrap()
            .unwrap();
        let mut inputs = Vec::new();
        for _ in 0..4 {
            inputs.push(writer.prepare_feature_input(&revision, &claim).unwrap());
        }
        assert!(matches!(
            writer.prepare_feature_input(&revision, &claim),
            Err(
                yield_library::sample_sources::SourceWriterError::FeatureInput(
                    SourceFeatureInputError::Busy
                )
            )
        ));
        inputs.pop();
        assert!(writer.prepare_feature_input(&revision, &claim).is_ok());
        assert_eq!(artifact_count(&fixture), 0);
    }

    #[test]
    fn feature_job_nested_original_input_computes_before_atomic_readiness() {
        let (fixture, mut writer, target) = setup_relative(Path::new("nested/tone.wav"));
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let computed = thread::spawn(move || job.compute(&AtomicBool::new(false)))
            .join()
            .unwrap();
        assert_eq!(status(&fixture), "running");
        assert_eq!(artifact_count(&fixture), 0);
        assert!(matches!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Published(_)
        ));
        assert_eq!(status(&fixture), "done");
        assert_eq!(artifact_count(&fixture), 1);
    }

    #[test]
    fn feature_job_nested_parent_relocation_refuses_original_input_without_readiness() {
        let (fixture, mut writer, target) = setup_relative(Path::new("nested/tone.wav"));
        let job = prepare_feature_job(&mut writer, &target, 10, 100)
            .unwrap()
            .unwrap();
        let nested = fixture.root.path().join("nested");
        let parked = fixture.root.path().join("parked");
        let original = fs::read(nested.join("tone.wav")).unwrap();
        fs::rename(&nested, &parked).unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("tone.wav"), &original).unwrap();
        let computed = job.compute(&AtomicBool::new(false));
        assert!(matches!(
            computed
                .finish(&mut writer, 11, &AtomicBool::new(false))
                .unwrap(),
            FeatureJobOutcome::Failed(FeatureJobFailure::Input(SourceFeatureInputError::Changed))
        ));
        assert_eq!(artifact_count(&fixture), 0);
        assert_eq!(status(&fixture), "failed");
        assert_eq!(fs::read(parked.join("tone.wav")).unwrap(), original);
        assert_eq!(fs::read(nested.join("tone.wav")).unwrap(), original);
    }
}
