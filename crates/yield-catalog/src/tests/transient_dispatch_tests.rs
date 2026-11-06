#[cfg(test)]
mod fixtures {
    use super::super::*;
    use yield_analysis::transients::TransientPreset;

    fn write_attacks(path: &Path, total_frames: u32, attacks: [u32; 2]) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..total_frames {
            let sample: i16 = if attacks.contains(&frame) { 24_000 } else { 0 };
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn setup() -> (Fixture, CatalogService, SourceId, u64) {
        let fixture = Fixture::new("transient-dispatch");
        write_attacks(
            &fixture.root.path().join("attacks.wav"),
            16_000,
            [1237, 9673],
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
