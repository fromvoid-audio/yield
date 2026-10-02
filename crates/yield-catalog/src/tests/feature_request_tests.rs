#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::{FeatureAnalysisBlocker, FeatureAnalysisCancellation, FeatureAnalysisState};
    use crate::{FeatureAnalysisPhase, FeaturePhaseMeasurement};
    use yield_library::sample_sources::SourceDatabaseConnectionRole;

    fn command(
        source: &SourceId,
        revision: u64,
        id: u64,
        cancellation: FeatureAnalysisCancellation,
    ) -> CatalogCommand {
        CatalogCommand::RequestFeatureAnalysis {
            source_id: source.clone(),
            lifecycle_generation: 1,
            expected_revision: revision,
            request_id: id,
            relative_path: "tone.wav".into(),
            cancellation,
        }
    }
    fn state(service: &CatalogService, id: u64) -> FeatureAnalysisState {
        match receive_until(
            service,
            |event| matches!(event, CatalogEvent::FeatureAnalysisState { request_id, .. } if *request_id == id),
        ) {
            CatalogEvent::FeatureAnalysisState { state, .. } => state,
            _ => unreachable!(),
        }
    }
    fn preparation(service: &CatalogService, id: u64, unfinished: bool) {
        let mut phases = vec![
            FeatureAnalysisPhase::InspectingManifest,
            FeatureAnalysisPhase::ComparingReadiness,
        ];
        if unfinished {
            phases.push(FeatureAnalysisPhase::QueueingUnfinishedWork);
        }
        for phase in phases {
            assert_eq!(
                phase.measurement(),
                match phase {
                    FeatureAnalysisPhase::InspectingManifest =>
                        FeaturePhaseMeasurement::Indeterminate,
                    _ => FeaturePhaseMeasurement::Targets {
                        completed: 0,
                        total: 1
                    },
                }
            );
            assert_eq!(
                state(service, id),
                FeatureAnalysisState::Preparing { phase }
            );
        }
    }
    #[test]
    fn feature_request_phase_reducer_noop_retry_and_terminal_are_monotonic() {
        use FeatureAnalysisPhase::*;
        let inspecting = FeatureAnalysisState::Preparing {
            phase: InspectingManifest,
        };
        let comparing = FeatureAnalysisState::Preparing {
            phase: ComparingReadiness,
        };
        let queueing = FeatureAnalysisState::Preparing {
            phase: QueueingUnfinishedWork,
        };
        let blocked = FeatureAnalysisState::Blocked {
            reason: FeatureAnalysisBlocker::NotClaimable,
        };
        assert!(inspecting.follows(None));
        assert!(comparing.follows(Some(&inspecting)));
        assert!(queueing.follows(Some(&comparing)));
        assert!(!inspecting.follows(Some(&comparing)));
        assert!(!comparing.follows(Some(&comparing)));
        assert!(blocked.follows(Some(&comparing))); // no queued-work promise needed
        assert!(!queueing.follows(Some(&blocked)));
        assert!(!inspecting.follows(Some(&blocked))); // retry needs a fresh request
        assert!(inspecting.follows(None)); // fresh request/source revision reset
        assert!(!FeatureAnalysisState::Queued.follows(Some(&FeatureAnalysisState::Running)));
    }
    fn assert_analysis_metrics(
        service: &CatalogService,
        expected: (u64, u64, u64, u64, u64, u64, u64, u64),
    ) -> FeatureAnalysisMetrics {
        // Event delivery can precede the worker's diagnostic update. Wait only for actual
        // settled observation; no injected outcome, counter, source or clock is accepted.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(m) = service.try_feature_analysis_metrics() {
                if m.dropped_before_work + m.terminal_ready + m.terminal_blocked >= expected.0 {
                    assert_eq!(
                        (
                            m.entered,
                            m.dropped_before_work,
                            m.terminal_ready,
                            m.terminal_blocked,
                            m.terminal_stale,
                            m.terminal_cancelled,
                            m.terminal_enqueued,
                            m.terminal_projection_rejected
                        ),
                        expected
                    );
                    assert_eq!(
                        m.entered,
                        m.dropped_before_work + m.terminal_ready + m.terminal_blocked
                    );
                    assert_eq!(
                        m.terminal_enqueued + m.terminal_projection_rejected,
                        m.terminal_ready + m.terminal_blocked
                    );
                    assert!(m.total_elapsed_us >= m.max_elapsed_us);
                    assert!(!m.saturated);
                    return m;
                }
            }
            assert!(
                Instant::now() < deadline,
                "actual feature worker did not settle"
            );
            thread::yield_now();
        }
    }
    fn await_removal_without_ready(service: &CatalogService, source: &SourceId, forbid_all: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = service
                .recv_event_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("actual source removal");
            assert!(
                !matches!(
                    &event,
                    CatalogEvent::FeatureAnalysisState {
                        state: FeatureAnalysisState::Ready { .. },
                        ..
                    }
                ),
                "retired ready escaped producer fence"
            );
            if forbid_all {
                assert!(!matches!(&event, CatalogEvent::FeatureAnalysisState { .. }));
            }
            if matches!(&event, CatalogEvent::Removed { source_id, .. } if source_id == source) {
                break;
            }
        }
        while let Ok(event) = service.try_recv_event() {
            assert!(!matches!(
                &event,
                CatalogEvent::FeatureAnalysisState {
                    state: FeatureAnalysisState::Ready { .. },
                    ..
                }
            ));
        }
    }
    fn observer(f: &Fixture, source: &SourceId) -> rusqlite::Connection {
        SourceDatabase::open_connection_with_role_and_database_root(
            f.root.path(),
            f.guard.profile_root().join(catalog_profile_child(source)),
            SourceDatabaseConnectionRole::UiRead,
        )
        .unwrap()
    }
    fn counts(f: &Fixture, source: &SourceId) -> (i64, i64) {
        observer(f, source)
            .query_row(
                "SELECT (SELECT count(*) FROM source_feature_artifacts),
            (SELECT count(*) FROM source_readiness_artifacts WHERE stage='analysis_features')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
    #[test]
    fn feature_request_production_catalog_audit_queue_compute_and_validated_ready() {
        let f = Fixture::new("production-feature-request");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("production-feature-request");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        assert_eq!(counts(&f, &source), (0, 0));
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 1, true);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Queued);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Running);
        let ready = state(&service, 1);
        assert!(matches!(ready, FeatureAnalysisState::Ready { .. }));
        assert_eq!(counts(&f, &source), (1, 1));
        let bytes: i64 = observer(&f, &source)
            .query_row(
                "SELECT length(payload) FROM source_feature_artifacts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, 732);
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                2,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 2, false);
        assert_eq!(state(&service, 2), ready);
        assert_eq!(counts(&f, &source), (1, 1));
        assert_analysis_metrics(&service, (2, 0, 2, 0, 0, 0, 2, 0));
        service.shutdown();
    }
    #[test]
    fn feature_request_invalid_engine_bytes_block_with_durable_retry_and_no_ready() {
        let f = Fixture::new("feature-request-invalid-wav");
        fs::write(f.root.path().join("tone.wav"), b"invalid real WAV bytes").unwrap();
        let source = SourceId::from_string("feature-request-invalid-wav");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 1, true);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Queued);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Running);
        assert_eq!(
            state(&service, 1),
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::Computation
            }
        );
        assert_eq!(counts(&f, &source), (0, 0));
        let retained: (String,String) = observer(&f,&source).query_row("SELECT status,failure_kind FROM analysis_jobs WHERE readiness_stage='analysis_features'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(retained, ("failed".into(), "retryable".into()));
        // A fresh request publishes preparation again. The actual clock may cross the
        // retained one-second deadline: either defer, or genuinely retry and fail decoding.
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                2,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 2, true);
        match state(&service, 2) {
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::NotClaimable,
            } => {}
            FeatureAnalysisState::Queued => {
                assert_eq!(state(&service, 2), FeatureAnalysisState::Running);
                assert_eq!(
                    state(&service, 2),
                    FeatureAnalysisState::Blocked {
                        reason: FeatureAnalysisBlocker::Computation,
                    }
                );
            }
            other => panic!("retry must defer or perform real failed computation: {other:?}"),
        }
        assert_eq!(counts(&f, &source), (0, 0));
        assert_analysis_metrics(&service, (2, 0, 0, 2, 0, 0, 2, 0));
        service.shutdown();
    }
    #[test]
    fn feature_request_cancelled_or_stale_revision_never_queue_or_complete() {
        let f = Fixture::new("feature-request-cancel-stale");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-request-cancel-stale");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        let cancellation = FeatureAnalysisCancellation::new();
        cancellation.cancel();
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                cancellation,
            ))
            .unwrap();
        assert_eq!(
            state(&service, 1),
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::Cancelled
            }
        );
        service
            .submit(command(
                &source,
                snapshot.committed_revision + 1,
                2,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        assert_eq!(
            state(&service, 2),
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::StaleSource
            }
        );
        assert_eq!(counts(&f, &source), (0, 0));
        let target_count: i64 = observer(&f, &source)
            .query_row("SELECT count(*) FROM source_readiness_targets", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(target_count, 0);
        assert_analysis_metrics(&service, (2, 0, 0, 2, 1, 1, 2, 0));
        service.shutdown();
    }
    #[test]
    fn feature_request_bounds_reject_invalid_relative_path_before_queue_admission() {
        let f = Fixture::new("feature-request-input-bounds");
        let service = CatalogService::start(&f.guard).unwrap();
        for path in [
            "../escape.wav".to_owned(),
            format!("{}.wav", "a".repeat(1025)),
        ] {
            let mut cmd = command(
                &SourceId::from_string("not-registered"),
                1,
                1,
                FeatureAnalysisCancellation::new(),
            );
            if let CatalogCommand::RequestFeatureAnalysis { relative_path, .. } = &mut cmd {
                *relative_path = path.into();
            }
            assert_eq!(
                service.submit(cmd),
                Err(CatalogSubmitError::InvalidFeatureRequest)
            );
        }
        service.shutdown();
    }
    #[test]
    fn feature_request_restart_retains_unexpired_claim_without_optimistic_ready() {
        use yield_library::sample_sources::readiness::{ReadinessClassification, ReadinessDeficit};
        let f = Fixture::new("feature-request-retained-claim");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-request-retained-claim");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        service.shutdown();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            f.root.path(),
            &f.guard,
            &catalog_profile_child(&source),
        )
        .unwrap();
        let mut writer = SourceWriter::new(db, source.clone(), 1).unwrap();
        let revision = writer.current_revision().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let target = writer
            .prepare_feature_readiness_target(
                &revision,
                Path::new("tone.wav"),
                now,
                &AtomicBool::new(false),
            )
            .unwrap();
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
        let old_claim = writer
            .claim_feature_artifact(&revision, &target, now, 120)
            .unwrap()
            .unwrap();
        drop(writer);
        drop(old_claim);
        let service = CatalogService::start(&f.guard).unwrap();
        service
            .submit(CatalogCommand::Register {
                source_id: source.clone(),
                canonical_root: f.root.path().into(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive_until(&service, |e| matches!(e, CatalogEvent::Registered { .. }));
        receive_until(&service, |e| {
            matches!(e, CatalogEvent::SnapshotReady { .. })
        });
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                2,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 2, false);
        assert_eq!(
            state(&service, 2),
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::NotClaimable
            }
        );
        assert_eq!(counts(&f, &source), (0, 0));
        service.shutdown();
    }
    #[test]
    fn feature_request_large_real_scanner_manifest_streams_complete_matrix_and_computes_real_payload()
     {
        let f = Fixture::new("large-production-feature-request");
        for i in 0..300 {
            write_test_wav(
                &f.root.path().join(format!("tone-{i:04}.wav")),
                16_000,
                16_000,
            );
        }
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("large-production-feature-request");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 1, true);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Queued);
        assert_eq!(state(&service, 1), FeatureAnalysisState::Running);
        assert!(matches!(
            state(&service, 1),
            FeatureAnalysisState::Ready { .. }
        ));
        assert_eq!(counts(&f, &source), (1, 1));
        let (targets,indexes):(i64,i64)=observer(&f,&source).query_row("SELECT (SELECT count(*) FROM source_readiness_targets),(SELECT count(*) FROM source_readiness_artifacts WHERE stage='indexed_identity')",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((targets, indexes), (904, 301));
        service.shutdown();
    }
    #[test]
    fn feature_request_rebuilds_missing_corrupt_and_unmarked_real_payload_after_restart() {
        use yield_library::sample_sources::readiness::FeatureArtifactFixtureFault as Fault;
        for fault in [
            Fault::MissingPayload,
            Fault::CorruptPayload,
            Fault::MissingMarker,
        ] {
            let f = Fixture::new("feature-request-real-rebuild");
            write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
            let source = SourceId::from_string("feature-request-real-rebuild");
            let service = CatalogService::start(&f.guard).unwrap();
            let snapshot = register_and_audit(&service, f.root.path(), &source);
            service
                .submit(command(
                    &source,
                    snapshot.committed_revision,
                    1,
                    FeatureAnalysisCancellation::new(),
                ))
                .unwrap();
            preparation(&service, 1, true);
            assert_eq!(state(&service, 1), FeatureAnalysisState::Queued);
            assert_eq!(state(&service, 1), FeatureAnalysisState::Running);
            let ready = state(&service, 1);
            assert!(matches!(ready, FeatureAnalysisState::Ready { .. }));
            let original: Vec<u8> = observer(&f, &source)
                .query_row("SELECT payload FROM source_feature_artifacts", [], |r| {
                    r.get(0)
                })
                .unwrap();
            service.shutdown();
            let db = SourceDatabase::open_for_catalog_with_profile_guard(
                f.root.path(),
                &f.guard,
                &catalog_profile_child(&source),
            )
            .unwrap();
            let mut writer = SourceWriter::new(db, source.clone(), 1).unwrap();
            let revision = writer.current_revision().unwrap();
            let target = writer
                .prepare_feature_readiness_target(
                    &revision,
                    Path::new("tone.wav"),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                    &AtomicBool::new(false),
                )
                .unwrap();
            // The fault hook only mutates this retained owner's exact current target in fixture
            // builds. Rebuild admission, held WAV decode/DSP and publication are production code.
            writer
                .inject_feature_artifact_fixture_fault(&revision, &target, fault)
                .unwrap();
            drop(writer);
            let service = CatalogService::start(&f.guard).unwrap();
            service
                .submit(CatalogCommand::Register {
                    source_id: source.clone(),
                    canonical_root: f.root.path().into(),
                    lifecycle_generation: 1,
                })
                .unwrap();
            receive_until(&service, |e| matches!(e, CatalogEvent::Registered { .. }));
            receive_until(&service, |e| {
                matches!(e, CatalogEvent::SnapshotReady { .. })
            });
            service
                .submit(command(
                    &source,
                    snapshot.committed_revision,
                    2,
                    FeatureAnalysisCancellation::new(),
                ))
                .unwrap();
            preparation(&service, 2, true);
            assert_eq!(state(&service, 2), FeatureAnalysisState::Queued);
            assert_eq!(state(&service, 2), FeatureAnalysisState::Running);
            assert_eq!(state(&service, 2), ready);
            assert_eq!(counts(&f, &source), (1, 1));
            let repaired: Vec<u8> = observer(&f, &source)
                .query_row("SELECT payload FROM source_feature_artifacts", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(repaired, original);
            service
                .submit(command(
                    &source,
                    snapshot.committed_revision,
                    3,
                    FeatureAnalysisCancellation::new(),
                ))
                .unwrap();
            preparation(&service, 3, false);
            assert_eq!(state(&service, 3), ready);
            service.shutdown();
        }
    }
    #[test]
    fn feature_request_changed_real_content_recomputes_and_preserves_old_immutable_payload() {
        let f = Fixture::new("feature-request-changed-content");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-request-changed-content");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 1, true);
        state(&service, 1);
        state(&service, 1);
        let old = state(&service, 1);
        assert!(matches!(old, FeatureAnalysisState::Ready { .. }));
        service.shutdown();
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 8_000);
        let service = CatalogService::start(&f.guard).unwrap();
        let next = register_and_audit(&service, f.root.path(), &source);
        assert!(next.committed_revision > snapshot.committed_revision);
        assert_eq!(next.source_id, source);
        assert_eq!(next.lifecycle_generation, snapshot.lifecycle_generation);
        // Before any new feature request refreshes desired targets, the changed manifest alone
        // invalidates the old completion. Its immutable bytes remain historical cache evidence.
        let connection = observer(&f, &source);
        let readiness = yield_library::sample_sources::readiness::ReadinessView::new(&connection)
            .reconcile(source.as_str(), 0)
            .unwrap();
        let old_target = readiness
            .entries
            .iter()
            .find(|entry| {
                entry.target.stage
                    == yield_library::sample_sources::readiness::ReadinessStage::AnalysisFeatures
            })
            .unwrap()
            .target
            .clone();
        assert_eq!(
            readiness
                .entries
                .iter()
                .find(|entry| entry.target == old_target)
                .unwrap()
                .classification,
            yield_library::sample_sources::readiness::ReadinessClassification::StaleByGeneration
        );
        assert_eq!(counts(&f, &source), (1, 1));
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                99,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        assert_eq!(
            state(&service, 99),
            FeatureAnalysisState::Blocked {
                reason: FeatureAnalysisBlocker::StaleSource
            }
        );
        assert_eq!(counts(&f, &source), (1, 1));
        service
            .submit(command(
                &source,
                next.committed_revision,
                2,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 2, true);
        assert_eq!(state(&service, 2), FeatureAnalysisState::Queued);
        assert_eq!(state(&service, 2), FeatureAnalysisState::Running);
        let new = state(&service, 2);
        assert!(matches!(new, FeatureAnalysisState::Ready { .. }));
        assert_ne!(new, old);
        assert_eq!(counts(&f, &source), (2, 1));
        let readiness = yield_library::sample_sources::readiness::ReadinessView::new(&connection)
            .reconcile(source.as_str(), 0)
            .unwrap();
        let entry = readiness
            .entries
            .iter()
            .find(|entry| {
                entry.target.stage
                    == yield_library::sample_sources::readiness::ReadinessStage::AnalysisFeatures
            })
            .unwrap();
        assert_eq!(
            entry.classification,
            yield_library::sample_sources::readiness::ReadinessClassification::Current
        );
        assert_ne!(
            entry.target.content_generation,
            old_target.content_generation
        );
        assert!(entry.target.source_generation > old_target.source_generation);
        assert_eq!(entry.target.scope_id, old_target.scope_id);
        service.shutdown();
        // Reopen once more: unchanged current content recovers verified readiness without DSP.
        let service = CatalogService::start(&f.guard).unwrap();
        service
            .submit(CatalogCommand::Register {
                source_id: source.clone(),
                canonical_root: f.root.path().into(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive_until(&service, |event| {
            matches!(event, CatalogEvent::Registered { .. })
        });
        receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        service
            .submit(command(
                &source,
                next.committed_revision,
                3,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        preparation(&service, 3, false);
        assert_eq!(state(&service, 3), new);
        assert_eq!(counts(&f, &source), (2, 1));
        service.shutdown();
    }
    #[test]
    fn feature_analysis_metrics_queued_retirement_drops_before_source_sql() {
        let f = Fixture::new("feature-metrics-queued-retirement");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-metrics-queued-retirement");
        let (service, gate) = CatalogService::start_with_test_gate(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        let before = counts(&f, &source);
        let pause = gate.pause_next();
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        service
            .submit(CatalogCommand::Remove {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        pause.release();
        await_removal_without_ready(&service, &source, true);
        assert_analysis_metrics(&service, (1, 1, 0, 0, 0, 0, 0, 0));
        assert_eq!(counts(&f, &source), before);
        service.shutdown();
    }
    #[test]
    fn feature_analysis_metrics_actual_ready_projection_rejected_after_retirement() {
        let f = Fixture::new("feature-metrics-terminal-retirement");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-metrics-terminal-retirement");
        let (service, gate) = CatalogService::start_with_test_gate(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        let pause = gate.arm_feature_terminal();
        service
            .submit(command(
                &source,
                snapshot.committed_revision,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        pause.wait_until_entered();
        // Actual scanner/WAV/DSP/SourceWriter publication already completed. Pause changes
        // only scheduling before the existing producer fence; no ready receipt is injected.
        let before = counts(&f, &source);
        assert_eq!(before, (1, 1));
        service
            .submit(CatalogCommand::Remove {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        drop(pause);
        await_removal_without_ready(&service, &source, false);
        assert_analysis_metrics(&service, (1, 0, 1, 0, 0, 0, 0, 1));
        assert_eq!(counts(&f, &source), before);
        service.shutdown();
    }
    #[test]
    fn feature_analysis_metrics_getter_is_nonblocking_readonly_and_resets_on_restart() {
        let f = Fixture::new("feature-metrics-observer");
        write_test_wav(&f.root.path().join("tone.wav"), 16_000, 16_000);
        let source = SourceId::from_string("feature-metrics-observer");
        let service = CatalogService::start(&f.guard).unwrap();
        let snapshot = register_and_audit(&service, f.root.path(), &source);
        service
            .submit(command(
                &source,
                snapshot.committed_revision + 1,
                1,
                FeatureAnalysisCancellation::new(),
            ))
            .unwrap();
        state(&service, 1);
        let metrics = assert_analysis_metrics(&service, (1, 0, 0, 1, 1, 0, 1, 0));
        let connection = observer(&f, &source);
        let file = PathBuf::from(connection.path().unwrap());
        let persisted = || {
            [
                file.clone(),
                PathBuf::from(format!("{}-wal", file.display())),
            ]
            .into_iter()
            .filter_map(|path| fs::read(&path).ok().map(|bytes| (path, bytes)))
            .collect::<Vec<_>>()
        };
        let before = persisted();
        assert_eq!(service.try_feature_analysis_metrics().unwrap(), metrics);
        assert_eq!(service.try_feature_analysis_metrics().unwrap(), metrics);
        assert_eq!(persisted(), before);
        let held = service.admission.lock().unwrap();
        assert!(service.try_feature_analysis_metrics().is_none());
        drop(held);
        drop(connection);
        service.shutdown();
        let service = CatalogService::start(&f.guard).unwrap();
        assert_eq!(
            service.try_feature_analysis_metrics().unwrap(),
            FeatureAnalysisMetrics::default()
        );
        service.shutdown();
    }
}
