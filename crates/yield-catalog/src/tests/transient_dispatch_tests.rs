#[cfg(test)]
mod fixtures {
    use super::super::*;
    use std::io::{Seek, SeekFrom, Write};
    use yield_analysis::transients::{DETECTOR_VERSION, STREAM_DETECTOR_VERSION, TransientPreset};

    fn write_attacks(path: &Path, total_frames: u32, attacks: [u32; 2]) {
        // Standard mono 16-bit PCM at 16 kHz. Sparse silence keeps test
        // provisioning small while the reader still consumes every frame.
        let data_bytes = total_frames * 2;
        let mut file = fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(data_bytes + 36).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data")
            .unwrap();
        file.write_all(&data_bytes.to_le_bytes()).unwrap();
        file.set_len(u64::from(data_bytes) + 44).unwrap();
        for attack in attacks {
            assert!(attack < total_frames);
            file.seek(SeekFrom::Start(44 + u64::from(attack) * 2))
                .unwrap();
            file.write_all(&24_000_i16.to_le_bytes()).unwrap();
        }
    }

    fn setup() -> (Fixture, CatalogService, SourceId, u64) {
        setup_attacks(16_000, [1237, 9673])
    }

    fn setup_attacks(
        total_frames: u32,
        attacks: [u32; 2],
    ) -> (Fixture, CatalogService, SourceId, u64) {
        let fixture = Fixture::new("transient-dispatch");
        write_attacks(
            &fixture.root.path().join("attacks.wav"),
            total_frames,
            attacks,
        );
        let source = SourceId::from_string("transient-dispatch");
        let service = CatalogService::start(&fixture.guard).unwrap();
        let snapshot = register_and_audit(&service, fixture.root.path(), &source);
        (fixture, service, source, snapshot.committed_revision)
    }

    fn delivery(service: &CatalogService, id: u64) -> CatalogDelivery {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(delivery) = service.recv_delivery_timeout(Duration::from_millis(50))
                && matches!(&delivery.event, CatalogEvent::TransientMarkersCompleted { request, .. } if request.request_id() == id)
            {
                return delivery;
            }
            assert!(Instant::now() < deadline, "missing transient completion");
        }
    }

    fn request(source: &SourceId, revision: u64, id: u64) -> SourceTransientRequest {
        SourceTransientRequest::new(
            source,
            1,
            revision,
            id,
            Path::new("attacks.wav"),
            TransientPreset::Normal,
        )
        .unwrap()
    }

    #[test]
    fn transient_worker_keeps_catalog_responsive_and_refunds_cancelled_and_busy_requests() {
        let fixture = Fixture::new("transient-compute-slot");
        write_attacks(
            &fixture.root.path().join("attacks.wav"),
            16_000,
            [1237, 9673],
        );
        let source = SourceId::from_string("transient-compute-slot");
        let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
        let revision =
            register_and_audit(&service, fixture.root.path(), &source).committed_revision;
        let (entered_send, entered_recv) = mpsc::sync_channel(1);
        let (release_send, release_recv) = mpsc::sync_channel(1);
        *gate.transient_compute.lock().unwrap() = Some((entered_send, release_recv));
        let cancellation = FeatureAnalysisCancellation::new();
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request(&source, revision, 1),
                cancellation: cancellation.clone(),
            })
            .unwrap();
        entered_recv.recv_timeout(Duration::from_secs(2)).unwrap();

        service
            .submit(CatalogCommand::ReadSnapshot {
                source_id: source.clone(),
                expected_revision: Some(revision),
            })
            .unwrap();
        receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });

        for id in [2, 3] {
            service
                .submit(CatalogCommand::RequestTransientMarkers {
                    request: request(&source, revision, id),
                    cancellation: FeatureAnalysisCancellation::new(),
                })
                .unwrap();
        }
        assert!(matches!(
            delivery(&service, 3).into_event_if_live(),
            Some(CatalogEvent::TransientMarkersCompleted {
                result: Err(TransientRequestFailure::Busy),
                ..
            })
        ));
        cancellation.cancel();
        release_send.send(()).unwrap();
        let CatalogEvent::TransientMarkersCompleted { result, .. } =
            delivery(&service, 2).into_event_if_live().unwrap()
        else {
            panic!("expected waiting request");
        };
        assert_eq!(result.unwrap().markers.frames(), [1237, 9673]);
        wait_for_tag_read_payload(&service, 0);
        wait_for_tag_page_input_release(&service);
        service.shutdown();
    }

    fn run_request(
        service: &CatalogService,
        source: &SourceId,
        revision: u64,
        id: u64,
        preset: TransientPreset,
    ) -> TransientJobOutcome {
        let request =
            SourceTransientRequest::new(source, 1, revision, id, Path::new("attacks.wav"), preset)
                .unwrap();
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request,
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        let CatalogEvent::TransientMarkersCompleted { result, .. } =
            delivery(service, id).into_event_if_live().unwrap()
        else {
            panic!("expected transient completion");
        };
        result.unwrap()
    }

    #[test]
    fn transient_cache_hits_after_reopen_and_misses_for_a_new_preset() {
        let (fixture, service, source, revision) = setup();
        let first = run_request(&service, &source, revision, 1, TransientPreset::Normal);
        assert_eq!(first.cache, TransientCacheUse::MissStored);
        assert_eq!(first.detector_version, DETECTOR_VERSION);
        assert_eq!(first.markers.frames(), [1237, 9673]);
        service.shutdown();

        let reopened = CatalogService::start(&fixture.guard).unwrap();
        let snapshot = register_and_audit(&reopened, fixture.root.path(), &source);
        let same = run_request(
            &reopened,
            &source,
            snapshot.committed_revision,
            2,
            TransientPreset::Normal,
        );
        assert_eq!(same.cache, TransientCacheUse::Hit);
        assert_eq!(same.markers.frames(), first.markers.frames());
        let high = run_request(
            &reopened,
            &source,
            snapshot.committed_revision,
            3,
            TransientPreset::High,
        );
        assert_eq!(high.cache, TransientCacheUse::MissStored);
        assert_eq!(high.markers.preset(), TransientPreset::High);
        reopened.shutdown();
    }

    #[test]
    fn transient_stream_routes_and_reuses_a_wav_above_the_encoded_limit() {
        let frames =
            (yield_library::sample_sources::readiness::FEATURE_INPUT_MAX_BYTES / 2) as u32 + 1024;
        let attacks = [32_767, 3_500_073];
        let (fixture, service, source, _) = setup_attacks(frames, attacks);
        service.shutdown();
        // Quick audit defers this file's digest. Complete the existing targeted
        // scanner hash under the same profile before reopening the Catalog.
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source),
        )
        .unwrap();
        yield_scan::complete_pending_deep_hash_for_path(&database, Path::new("attacks.wav"), None)
            .unwrap();
        drop(database);
        let service = CatalogService::start(&fixture.guard).unwrap();
        let revision =
            register_and_audit(&service, fixture.root.path(), &source).committed_revision;
        let first = run_request(&service, &source, revision, 1, TransientPreset::Normal);
        assert_eq!(first.cache, TransientCacheUse::MissStored);
        assert_eq!(first.detector_version, STREAM_DETECTOR_VERSION);
        assert_eq!(first.markers.source_frames(), frames);
        assert_eq!(first.markers.frames(), attacks);
        let cached = run_request(&service, &source, revision, 2, TransientPreset::Normal);
        assert_eq!(cached.cache, TransientCacheUse::Hit);
        assert_eq!(cached.detector_version, STREAM_DETECTOR_VERSION);
        assert_eq!(cached.markers.frames(), attacks);
        service.shutdown();
    }

    #[test]
    fn transient_stream_reports_a_pending_quick_audit_digest() {
        use yield_library::sample_sources::readiness::{
            FEATURE_INPUT_MAX_BYTES, SourceFeatureInputError,
        };
        let frames = (FEATURE_INPUT_MAX_BYTES / 2) as u32 + 1024;
        let (_fixture, service, source, revision) = setup_attacks(frames, [32_767, 3_500_073]);
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request(&source, revision, 1),
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        assert!(matches!(
            delivery(&service, 1).into_event_if_live(),
            Some(CatalogEvent::TransientMarkersCompleted {
                result: Err(TransientRequestFailure::Input(
                    SourceFeatureInputError::ContentPending
                )),
                ..
            })
        ));
        service.shutdown();
    }

    #[test]
    fn transient_stream_falls_back_when_small_encoded_input_exceeds_the_pcm_limit() {
        let attacks = [32_768, 1_950_073];
        let (_fixture, service, source, revision) = setup_attacks(2_000_200, attacks);
        let result = run_request(&service, &source, revision, 1, TransientPreset::Normal);
        assert_eq!(result.detector_version, STREAM_DETECTOR_VERSION);
        assert_eq!(result.markers.frames(), attacks);
        service.shutdown();
    }

    #[test]
    fn transient_cache_recomputes_after_audited_content_changes() {
        let (fixture, service, source, revision) = setup();
        let first = run_request(&service, &source, revision, 1, TransientPreset::Normal);
        assert_eq!(first.cache, TransientCacheUse::MissStored);
        let file = fixture.root.path().join("attacks.wav");
        write_attacks(&file, 17_000, [2041, 10_779]);
        let new_bytes = fs::read(&file).unwrap();
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        let CatalogEvent::AuditCommitted { snapshot } = receive_until(&service, |event| {
            matches!(event, CatalogEvent::AuditCommitted { .. })
        }) else {
            unreachable!();
        };
        assert!(snapshot.committed_revision > revision);
        let result = run_request(
            &service,
            &source,
            snapshot.committed_revision,
            2,
            TransientPreset::Normal,
        );
        assert_eq!(result.cache, TransientCacheUse::MissStored);
        assert_eq!(result.markers.frames(), [2041, 10_779]);
        assert_eq!(fs::read(&file).unwrap(), new_bytes);
        service.shutdown();
    }

    #[test]
    fn transient_dispatch_returns_exact_frames_and_refunds_payload_credit() {
        let (_fixture, service, source, revision) = setup();
        let request = request(&source, revision, 1);
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request.clone(),
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        wait_for_tag_read_payload(&service, transient_dispatch::MAX_RESULT_BYTES);
        let CatalogEvent::TransientMarkersCompleted {
            request: delivered,
            result,
            ..
        } = delivery(&service, 1).into_event_if_live().unwrap()
        else {
            panic!("expected transient completion");
        };
        assert_eq!(delivered, request);
        assert_eq!(result.unwrap().markers.frames(), [1237, 9673]);
        wait_for_tag_read_payload(&service, 0);
        wait_for_tag_page_input_release(&service);
        service.shutdown();
    }

    #[test]
    fn transient_dispatch_refuses_stale_cancelled_and_retired_results() {
        let (_fixture, service, source, revision) = setup();
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request(&source, revision + 1, 1),
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        assert!(matches!(
            delivery(&service, 1).into_event_if_live(),
            Some(CatalogEvent::TransientMarkersCompleted {
                result: Err(TransientRequestFailure::StaleSource),
                ..
            })
        ));

        let cancellation = FeatureAnalysisCancellation::new();
        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request(&source, revision, 2),
                cancellation: cancellation.clone(),
            })
            .unwrap();
        let completed = delivery(&service, 2);
        assert!(matches!(
            &completed.event,
            CatalogEvent::TransientMarkersCompleted { result: Ok(_), .. }
        ));
        cancellation.cancel();
        assert!(completed.into_event_if_live().is_none());

        service
            .submit(CatalogCommand::RequestTransientMarkers {
                request: request(&source, revision, 3),
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        let completed = delivery(&service, 3);
        service
            .submit(CatalogCommand::Remove {
                source_id: source,
                lifecycle_generation: 1,
            })
            .unwrap();
        let retirement = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::Removed { .. }
                    | CatalogEvent::Failed {
                        operation: CatalogOperation::Remove,
                        ..
                    }
            )
        });
        assert!(
            matches!(retirement, CatalogEvent::Removed { .. }),
            "{retirement:?}"
        );
        assert!(completed.into_event_if_live().is_none());
        wait_for_tag_read_payload(&service, 0);
        service.shutdown();
    }

    #[test]
    fn transient_dispatch_rejects_invalid_path_and_request_id_at_construction() {
        let source = SourceId::from_string("source");
        for (id, path) in [(1, "../escape.wav"), (0, "attacks.wav")] {
            assert_eq!(
                SourceTransientRequest::new(
                    &source,
                    1,
                    1,
                    id,
                    Path::new(path),
                    TransientPreset::Normal,
                ),
                Err(TransientRequestFailure::InvalidInput),
            );
        }
    }
}
