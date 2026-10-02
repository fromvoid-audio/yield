#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::{
        SimilarityAspect, SimilarityCandidateOutcome, SimilarityQueryError, SourceSimilarityQuery,
        query_source_similarity,
    };
    use yield_library::sample_sources::readiness::{
        FeatureArtifactPayload, ReadinessClassification, ReadinessDeficit, ReadinessTarget,
    };

    fn setup() -> (Fixture, SourceWriter, Vec<ReadinessTarget>) {
        setup_with_vector(|_, vector| vector)
    }
    fn setup_with_vector(
        transform: impl Fn(usize, Vec<f32>) -> Vec<f32>,
    ) -> (Fixture, SourceWriter, Vec<ReadinessTarget>) {
        let fixture = Fixture::new("similarity-query");
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            Path::new("similarity-query-db"),
        )
        .unwrap();
        let writer =
            SourceWriter::new_for_scan(db, SourceId::from_string("similarity-source"), 1).unwrap();
        let root = writer.current_scan_fence().unwrap().root_identity().clone();
        let db = writer.into_catalog_database();
        let mut batch = db.write_batch().unwrap();
        for (index, path) in ["anchor.wav", "same.wav", "other.wav", "missing.wav"]
            .iter()
            .enumerate()
        {
            let file = fixture.root.path().join(path);
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
            let frequency = if index < 2 { 0.1 } else { 0.23 };
            for i in 0..4000 {
                wav.write_sample::<i16>(((i as f32 * frequency).sin() * 12000.0) as i16)
                    .unwrap();
            }
            wav.finalize().unwrap();
            let meta = file.metadata().unwrap();
            let modified = i64::try_from(
                meta.modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            )
            .unwrap();
            batch
                .upsert_file_with_hash(
                    Path::new(path),
                    meta.len(),
                    modified,
                    &blake3::hash(&fs::read(&file).unwrap()).to_hex().to_string(),
                )
                .unwrap();
            batch
                .set_file_identity(
                    Path::new(path),
                    Some(&stable_filesystem_identity(&file, &meta).unwrap()),
                )
                .unwrap();
        }
        batch.commit().unwrap();
        let revision = db.get_revision().unwrap();
        db.begin_source_directory_truth_generation(1, 0).unwrap();
        db.write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(1, revision, 4, root, None)
            .unwrap();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("similarity-source"), 1).unwrap();
        let revision = writer.current_revision().unwrap();
        let cancel = AtomicBool::new(false);
        let mut targets = Vec::new();
        for (index, path) in ["anchor.wav", "same.wav", "other.wav", "missing.wav"]
            .iter()
            .enumerate()
        {
            let target = writer
                .prepare_feature_readiness_target(&revision, Path::new(path), 1, &cancel)
                .unwrap();
            if index < 3 {
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
                let claim = writer
                    .claim_feature_artifact(&revision, &target, 10, 100)
                    .unwrap()
                    .unwrap();
                let vector = yield_analysis::compute_feature_vector_v1_for_path(
                    &fixture.root.path().join(path),
                )
                .unwrap();
                let payload =
                    FeatureArtifactPayload::from_v1_vector(&transform(index, vector)).unwrap();
                writer
                    .publish_feature_artifact(&revision, &claim, &payload, 11, &cancel)
                    .unwrap();
            }
            targets.push(target);
        }
        (fixture, writer, targets)
    }
    fn query(
        writer: &SourceWriter,
        targets: &[ReadinessTarget],
        aspect: SimilarityAspect,
        generation: u64,
    ) -> SourceSimilarityQuery {
        SourceSimilarityQuery::new(
            writer.current_revision().unwrap(),
            targets[0].clone(),
            aspect,
            generation,
            &targets[1..],
        )
        .unwrap()
    }

    #[test]
    fn similarity_query_uses_actual_owner_payloads_and_existing_aspect_scores_without_writes() {
        let (fixture, mut writer, targets) = setup();
        let request = query(&writer, &targets, SimilarityAspect::Spectrum, 1);
        let directory = fixture.guard.profile_root().join("similarity-query-db");
        let retained: Vec<_> = [".yield.db", ".yield.db-wal"]
            .into_iter()
            .filter_map(|name| {
                fs::read(directory.join(name))
                    .ok()
                    .map(|bytes| (name, bytes))
            })
            .collect();
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        assert!(result.matches_query(&request));
        assert!(result.is_current_for(&request));
        assert_eq!(result.rows().len(), 3);
        assert_eq!(
            result.rows()[0].target().relative_path.as_deref(),
            Some("same.wav")
        );
        assert!(
            matches!(result.rows()[0].outcome(), SimilarityCandidateOutcome::Scored(score) if (*score - 1.0).abs() < 1e-5)
        );
        assert_eq!(
            result.rows()[2].outcome(),
            &SimilarityCandidateOutcome::Unavailable
        );
        assert!(result.rows()[2].key().is_none());
        assert_eq!(result.rows()[2].aspect_scores(), &[None; 5]);
        for aspect in SimilarityAspect::ORDER {
            let score = result.rows()[0]
                .aspect_score(aspect)
                .expect("real WAV aspect available");
            assert!(score.is_finite() && (-1.0..=1.0).contains(&score));
            assert!((score - 1.0).abs() < 1e-5);
        }
        assert!(!result.matches_query(&query(&writer, &targets, SimilarityAspect::Amplitude, 1)));
        assert!(!result.matches_query(&query(&writer, &targets, SimilarityAspect::Spectrum, 2)));
        assert_eq!(writer.current_revision().unwrap(), *request.revision());
        assert!(
            !retained.is_empty(),
            "actual retained DB/WAL bytes must be inspected"
        );
        for (name, before) in retained {
            assert_eq!(fs::read(directory.join(name)).unwrap(), before);
        }
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_retains_partial_aspects_and_preserves_selected_anchor_failure() {
        let (_fixture, mut writer, targets) = setup_with_vector(|index, mut vector| {
            if index < 2 {
                // Zero just the amplitude slice in anchor and identical candidate.
                for slot in [0, 1, 2, 3, 5, 6, 7, 8] {
                    vector[slot] = 0.0;
                }
            }
            vector
        });
        let request = query(&writer, &targets, SimilarityAspect::Spectrum, 1);
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        let same = result
            .rows()
            .iter()
            .find(|row| row.target() == &targets[1])
            .unwrap();
        assert_eq!(same.aspect_score(SimilarityAspect::Amplitude), None);
        for aspect in [
            SimilarityAspect::Overall,
            SimilarityAspect::Spectrum,
            SimilarityAspect::Timbre,
            SimilarityAspect::Pitch,
        ] {
            assert!((same.aspect_score(aspect).unwrap() - 1.0).abs() < 1e-5);
        }
        assert_eq!(
            same.outcome(),
            &SimilarityCandidateOutcome::Scored(
                same.aspect_score(SimilarityAspect::Spectrum).unwrap()
            )
        );
        let amplitude = query(&writer, &targets, SimilarityAspect::Amplitude, 2);
        assert!(matches!(
            query_source_similarity(&mut writer, &amplitude, &AtomicBool::new(false)),
            Err(SimilarityQueryError::AnchorUnavailable)
        ));
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_invalid_candidate_aspects_do_not_hide_other_valid_scores() {
        let (_fixture, mut writer, targets) = setup_with_vector(|index, mut vector| {
            if index == 1 {
                for slot in [0, 1, 2, 3, 5, 6, 7, 8] {
                    vector[slot] = 0.0;
                }
            } else if index == 2 {
                vector.fill(0.0);
            }
            vector
        });
        let request = query(&writer, &targets, SimilarityAspect::Amplitude, 1);
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        let partial = result
            .rows()
            .iter()
            .find(|row| row.target() == &targets[1])
            .unwrap();
        assert_eq!(partial.outcome(), &SimilarityCandidateOutcome::Unavailable);
        assert_eq!(partial.aspect_score(SimilarityAspect::Amplitude), None);
        assert!((partial.aspect_score(SimilarityAspect::Spectrum).unwrap() - 1.0).abs() < 1e-5);
        assert!(partial.key().is_some());
        let zero = result
            .rows()
            .iter()
            .find(|row| row.target() == &targets[2])
            .unwrap();
        assert_eq!(zero.aspect_scores(), &[None; 5]);
        assert!(zero.key().is_some()); // Valid payload with no normalized descriptor.
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_all_aspects_preserve_ties_and_missing_completion_is_unavailable() {
        use yield_library::sample_sources::readiness::FeatureArtifactFixtureFault;
        let (_fixture, mut writer, targets) = setup_with_vector(|_, _| vec![1.0; 183]);
        for aspect in SimilarityAspect::ORDER {
            let request = query(&writer, &targets, aspect, 1);
            let result =
                query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
            assert!(result.matches_query(&request));
            assert_eq!(result.rows()[0].target(), &targets[2]); // other.wav precedes same.wav on equal scores.
            assert_eq!(result.rows()[1].target(), &targets[1]);
            for row in &result.rows()[..2] {
                assert_eq!(
                    row.outcome(),
                    &SimilarityCandidateOutcome::Scored(row.aspect_score(aspect).unwrap())
                );
                for value in row.aspect_scores() {
                    assert!((value.unwrap() - 1.0).abs() < 1e-5);
                }
            }
        }
        let request = query(&writer, &targets, SimilarityAspect::Overall, 2);
        writer
            .inject_feature_artifact_fixture_fault(
                request.revision(),
                &targets[1],
                FeatureArtifactFixtureFault::MissingMarker,
            )
            .unwrap();
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        let stale = result
            .rows()
            .iter()
            .find(|row| row.target() == &targets[1])
            .unwrap();
        assert_eq!(stale.aspect_scores(), &[None; 5]);
        assert!(stale.key().is_none());
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_rejects_cancelled_stale_and_unavailable_anchor() {
        let (_fixture, mut writer, targets) = setup();
        let request = query(&writer, &targets, SimilarityAspect::Timbre, 1);
        assert!(matches!(
            query_source_similarity(&mut writer, &request, &AtomicBool::new(true)),
            Err(SimilarityQueryError::Cancelled)
        ));
        let missing = SourceSimilarityQuery::new(
            writer.current_revision().unwrap(),
            targets[3].clone(),
            SimilarityAspect::Overall,
            2,
            &targets[..2],
        )
        .unwrap();
        assert!(matches!(
            query_source_similarity(&mut writer, &missing, &AtomicBool::new(false)),
            Err(SimilarityQueryError::AnchorUnavailable)
        ));
        let db = writer.into_catalog_database();
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("similarity-source"), 2).unwrap();
        assert!(matches!(
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)),
            Err(SimilarityQueryError::Stale)
        ));
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_validates_full_inputs_before_owner_reads() {
        let (_fixture, writer, targets) = setup();
        let revision = writer.current_revision().unwrap();
        for candidates in [
            vec![targets[0].clone()],
            vec![targets[1].clone(); 2],
            vec![targets[1].clone(); 65],
        ] {
            assert!(matches!(
                SourceSimilarityQuery::new(
                    revision.clone(),
                    targets[0].clone(),
                    SimilarityAspect::Overall,
                    1,
                    &candidates
                ),
                Err(SimilarityQueryError::InvalidInput)
            ));
        }
        for edit in 0..6 {
            let mut target = targets[1].clone();
            match edit {
                0 => target.source_id = "foreign-source".into(),
                1 => target.relative_path = Some("../escape.wav".into()),
                2 => target.scope_id = "x".repeat(1025),
                3 => target.content_generation = "wrong".into(),
                4 => target.required_version = "unknown".into(),
                _ => target.relative_path = Some("x".repeat(4097)),
            }
            assert!(matches!(
                SourceSimilarityQuery::new(
                    revision.clone(),
                    targets[0].clone(),
                    SimilarityAspect::Overall,
                    1,
                    &[target]
                ),
                Err(SimilarityQueryError::InvalidInput)
            ));
        }
        assert!(
            SourceSimilarityQuery::new(
                revision,
                targets[0].clone(),
                SimilarityAspect::Overall,
                0,
                &[]
            )
            .is_err()
        );
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_corruption_is_unavailable_and_restart_requires_new_owner_fence() {
        use yield_library::sample_sources::readiness::FeatureArtifactFixtureFault;
        let (_fixture, mut writer, targets) = setup();
        let request = query(&writer, &targets, SimilarityAspect::Pitch, 1);
        let first =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        writer
            .inject_feature_artifact_fixture_fault(
                request.revision(),
                &targets[1],
                FeatureArtifactFixtureFault::CorruptPayload,
            )
            .unwrap();
        let after =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        assert!(after.rows().iter().any(|row| row.target() == &targets[1]
            && row.outcome() == &SimilarityCandidateOutcome::Unavailable
            && row.aspect_scores() == &[None; 5]));
        let db = writer.into_catalog_database();
        assert!(
            !first.is_current_for(&request),
            "retired writer cannot keep old results current"
        );
        let mut writer =
            SourceWriter::new(db, SourceId::from_string("similarity-source"), 1).unwrap();
        let fresh = query(&writer, &targets, SimilarityAspect::Pitch, 2);
        assert!(!first.matches_query(&fresh));
        assert!(query_source_similarity(&mut writer, &fresh, &AtomicBool::new(false)).is_ok());
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_rejects_forged_path_content_and_output_context() {
        let (_fixture, mut writer, targets) = setup();
        let request = query(&writer, &targets, SimilarityAspect::Overall, 1);
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        let mut changed = targets.clone();
        changed[1].relative_path = Some("other.wav".into());
        changed[2].relative_path = Some("same.wav".into());
        let forged = query(&writer, &changed, SimilarityAspect::Overall, 1);
        assert!(!result.is_current_for(&forged));
        assert!(matches!(
            query_source_similarity(&mut writer, &forged, &AtomicBool::new(false)),
            Err(SimilarityQueryError::Stale)
        ));
        changed = targets.clone();
        changed[1].content_generation = "0".repeat(64);
        let stale = query(&writer, &changed, SimilarityAspect::Overall, 1);
        assert!(!result.is_current_for(&stale));
        assert!(matches!(
            query_source_similarity(&mut writer, &stale, &AtomicBool::new(false)),
            Err(SimilarityQueryError::Stale)
        ));
        writer.close().unwrap();
        assert!(!result.is_current_for(&request));
    }

    #[test]
    fn similarity_query_constructor_does_not_retain_oversized_anchor_allocations() {
        let (_fixture, mut writer, targets) = setup();
        let mut anchor = targets[0].clone();
        for text in [
            &mut anchor.source_id,
            &mut anchor.scope_id,
            &mut anchor.content_generation,
            &mut anchor.required_version,
            anchor.relative_path.as_mut().unwrap(),
        ] {
            text.reserve(2 * 1024 * 1024);
        }
        let request = SourceSimilarityQuery::new(
            writer.current_revision().unwrap(),
            anchor,
            SimilarityAspect::Spectrum,
            1,
            &targets[1..],
        )
        .unwrap();
        assert_eq!(request.anchor(), &targets[0]);
        let retained = request.anchor();
        for text in [
            &retained.source_id,
            &retained.scope_id,
            &retained.content_generation,
            &retained.required_version,
            retained.relative_path.as_ref().unwrap(),
        ] {
            assert_eq!(text.capacity(), text.len());
        }
        let result =
            query_source_similarity(&mut writer, &request, &AtomicBool::new(false)).unwrap();
        assert!(result.matches_query(&request));
        writer.close().unwrap();
    }

    #[test]
    fn similarity_query_rejects_combined_path_budget_and_noncanonical_paths() {
        let (_fixture, writer, targets) = setup();
        let revision = writer.current_revision().unwrap();
        let candidates: Vec<_> = (0..17)
            .map(|index| {
                let mut target = targets[1].clone();
                target.scope_id = format!("identity-{index}");
                target.relative_path = Some(format!("{index:02}{}", "x".repeat(4090)));
                target
            })
            .collect();
        assert!(
            SourceSimilarityQuery::new(
                revision.clone(),
                targets[0].clone(),
                SimilarityAspect::Overall,
                1,
                &candidates
            )
            .is_err()
        );
        for path in ["./same.wav", "dir//same.wav", "dir\\same.wav", "bad\0.wav"] {
            let mut target = targets[1].clone();
            target.relative_path = Some(path.into());
            assert!(
                SourceSimilarityQuery::new(
                    revision.clone(),
                    targets[0].clone(),
                    SimilarityAspect::Overall,
                    1,
                    &[target]
                )
                .is_err()
            );
        }
        writer.close().unwrap();
    }
}
