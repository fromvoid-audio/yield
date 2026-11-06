#[cfg(test)]
mod fixtures {
    use super::super::*;
    use yield_analysis::transients::TransientPreset;

    fn setup() -> (Fixture, CatalogService, SourceId, u64) {
        let fixture = Fixture::new("transient-dispatch");
        let mut writer = hound::WavWriter::create(
            fixture.root.path().join("attacks.wav"),
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..16_000 {
            let sample: i16 = match frame {
                1237 => 24_000,
                9673 => -24_000,
                _ => 0,
            };
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
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
        assert_eq!(result.unwrap().frames(), [1237, 9673]);
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
