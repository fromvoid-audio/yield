#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::{
        SimilarityAspect, SimilarityCancellation, SimilarityCandidateOutcome,
        SimilarityDispatchFailure, SourceSimilarityRequest,
    };

    fn request(source: &SourceId, revision: u64, generation: u64) -> SourceSimilarityRequest {
        SourceSimilarityRequest::new(
            source.clone(),
            1,
            revision,
            generation,
            "anchor.wav".into(),
            &["same.wav".into(), "missing.wav".into()],
            SimilarityAspect::Spectrum,
        )
        .unwrap()
    }
    fn submit(
        service: &CatalogService,
        request: SourceSimilarityRequest,
        token: SimilarityCancellation,
    ) {
        service
            .submit(CatalogCommand::RequestSimilarity {
                request,
                cancellation: token,
            })
            .unwrap();
    }
    fn delivery(service: &CatalogService, generation: u64) -> CatalogDelivery {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let delivery = service.recv_delivery_timeout(Duration::from_millis(50));
            if let Ok(delivery) = delivery {
                if matches!(&delivery.event, CatalogEvent::SimilarityCompleted { request, .. } if request.generation() == generation)
                {
                    return delivery;
                }
            }
            assert!(Instant::now() < deadline, "missing similarity completion");
        }
    }
    fn setup() -> (Fixture, CatalogService, SourceId, CatalogSnapshot) {
        setup_with_candidates(2)
    }
    fn setup_with_candidates(count: usize) -> (Fixture, CatalogService, SourceId, CatalogSnapshot) {
        let fixture = Fixture::new("similarity-dispatch");
        for path in ["anchor.wav", "same.wav", "missing.wav"] {
            write_test_wav(&fixture.root.path().join(path), 16_000, 4000);
        }
        for index in 2..count {
            write_test_wav(
                &fixture
                    .root
                    .path()
                    .join(format!("candidate-{index:02}.wav")),
                16_000,
                4000,
            );
        }
        let source = SourceId::from_string("similarity-dispatch");
        let service = CatalogService::start(&fixture.guard).unwrap();
        let snapshot = register_and_audit(&service, fixture.root.path(), &source);
        for (index, path) in ["anchor.wav", "same.wav"].into_iter().enumerate() {
            service
                .submit(CatalogCommand::RequestFeatureAnalysis {
                    source_id: source.clone(),
                    lifecycle_generation: 1,
                    expected_revision: snapshot.committed_revision,
                    request_id: index as u64 + 1,
                    relative_path: path.into(),
                    cancellation: FeatureAnalysisCancellation::new(),
                })
                .unwrap();
            let event = receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::FeatureAnalysisState {
                        state: FeatureAnalysisState::Ready { .. }
                            | FeatureAnalysisState::Blocked { .. },
                        ..
                    }
                )
            });
            assert!(
                matches!(
                    event,
                    CatalogEvent::FeatureAnalysisState {
                        state: FeatureAnalysisState::Ready { .. },
                        ..
                    }
                ),
                "{event:?}"
            );
        }
        (fixture, service, source, snapshot)
    }
    fn drained(service: &CatalogService) {
        wait_for_tag_page_input_release(service);
        while service.try_recv_delivery().is_ok() {}
        wait_for_tag_read_payload(service, 0);
    }
    #[test]
    fn similarity_dispatch_real_catalog_scores_exact_set_and_releases_credits() {
        let (fixture, service, source, snapshot) = setup();
        let directory = fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&source));
        let bytes: Vec<_> = [".yield.db", ".yield.db-wal"]
            .into_iter()
            .filter_map(|name| fs::read(directory.join(name)).ok().map(|data| (name, data)))
            .collect();
        let request = request(&source, snapshot.committed_revision, 10);
        submit(&service, request.clone(), SimilarityCancellation::new());
        let (event, fence) = delivery(&service, 10).into_parts_if_live().unwrap();
        assert!(fence.unwrap().is_live());
        let CatalogEvent::SimilarityCompleted {
            request: captured,
            result: Ok(result),
            ..
        } = event
        else {
            panic!("{event:?}")
        };
        assert_eq!(captured, request);
        assert_eq!(result.query().generation(), 10);
        assert_eq!(result.query().aspect(), SimilarityAspect::Spectrum);
        assert_eq!(
            result.query().revision().revision(),
            snapshot.committed_revision
        );
        assert_eq!(result.rows().len(), 2);
        assert!(
            matches!(result.rows()[0].outcome(), SimilarityCandidateOutcome::Scored(score) if (*score - 1.0).abs() < 0.0001)
        );
        assert_eq!(
            result.rows()[1].outcome(),
            &SimilarityCandidateOutcome::Unavailable
        );
        for aspect in SimilarityAspect::ORDER {
            assert!((result.rows()[0].aspect_score(aspect).unwrap() - 1.0).abs() < 0.0001);
        }
        assert_eq!(result.rows()[1].aspect_scores(), &[None; 5]);
        for (name, previous) in bytes {
            assert_eq!(fs::read(directory.join(name)).unwrap(), previous);
        }
        drained(&service);
        service.shutdown();
    }
    #[test]
    fn similarity_dispatch_maximum_actual_rows_keep_fixed_result_credit() {
        let (_fixture, service, source, snapshot) = setup_with_candidates(64);
        drained(&service);
        let candidates: Vec<String> = ["same.wav".to_owned(), "missing.wav".to_owned()]
            .into_iter()
            .chain((2..64).map(|index| format!("candidate-{index:02}.wav")))
            .collect();
        let request = SourceSimilarityRequest::new(
            source,
            1,
            snapshot.committed_revision,
            40,
            "anchor.wav".into(),
            &candidates,
            SimilarityAspect::Spectrum,
        )
        .unwrap();
        submit(&service, request, SimilarityCancellation::new());
        // Real retained result, not a preexecution error or fabricated row fixture.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = service
                .events
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .events
                .iter()
                .any(|queued| {
                    matches!(&queued.event,
                    CatalogEvent::SimilarityCompleted { request, .. } if request.generation() == 40)
                });
            if ready {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(
            admission_state(&service.admission, |state| state.tag_read_payload_bytes),
            crate::similarity_dispatch::MAX_RESULT_BYTES
        );
        let event = delivery(&service, 40).into_event_if_live().unwrap();
        let CatalogEvent::SimilarityCompleted {
            result: Ok(result), ..
        } = event
        else {
            panic!("{event:?}")
        };
        assert_eq!(result.rows().len(), 64);
        assert!(result.rows()[0].aspect_scores().iter().all(Option::is_some));
        assert!(
            result.rows()[1..]
                .iter()
                .all(|row| row.aspect_scores() == &[None; 5])
        );
        drained(&service);
        assert_eq!(
            admission_state(&service.admission, |state| state.tag_read_payload_bytes),
            0
        );
        service.shutdown();
    }

    #[test]
    fn similarity_dispatch_read_does_not_renew_near_expiry_or_expired_owner() {
        let (fixture, service, source, snapshot) = setup();
        let directory = fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&source));
        // Deliberate fixture-only perturbation of the actual retained lease row.
        let sql = rusqlite::Connection::open_with_flags(
            directory.join(".yield.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .unwrap();
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        sql.execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 30],
        )
        .unwrap();
        let before: Vec<_> = [".yield.db", ".yield.db-wal"]
            .into_iter()
            .filter_map(|name| {
                fs::read(directory.join(name))
                    .ok()
                    .map(|bytes| (name, bytes))
            })
            .collect();
        submit(
            &service,
            request(&source, snapshot.committed_revision, 16),
            SimilarityCancellation::new(),
        );
        assert!(matches!(
            delivery(&service, 16).into_event_if_live().unwrap(),
            CatalogEvent::SimilarityCompleted { result: Ok(_), .. }
        ));
        let expiry: i64 = sql
            .query_row(
                "SELECT expires_at FROM source_lease WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            expiry,
            now + 30,
            "similarity renewed lease through control writer"
        );
        for (name, bytes) in before {
            assert!(
                fs::read(directory.join(name)).unwrap() == bytes,
                "similarity changed {name}"
            );
        }
        sql.execute(
            "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
            [],
        )
        .unwrap();
        let expiry: i64 = sql
            .query_row(
                "SELECT expires_at FROM source_lease WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        submit(
            &service,
            request(&source, snapshot.committed_revision, 17),
            SimilarityCancellation::new(),
        );
        let (event, fence) = delivery(&service, 17).into_parts_if_live().unwrap();
        assert!(fence.is_none()); // diagnostic, never a source-qualified successful projection
        assert!(matches!(
            event,
            CatalogEvent::SimilarityCompleted {
                result: Err(SimilarityDispatchFailure::StaleSource),
                ..
            }
        ));
        drained(&service);
        assert_eq!(
            sql.query_row(
                "SELECT expires_at FROM source_lease WHERE singleton=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            expiry
        );
        drop(sql);
        service.shutdown();
    }

    #[test]
    fn similarity_dispatch_queued_cancellation_drops_projection_without_sql() {
        let (_fixture, service, source, snapshot) = setup();
        let token = SimilarityCancellation::new();
        submit(
            &service,
            request(&source, snapshot.committed_revision, 11),
            token.clone(),
        );
        let queued = delivery(&service, 11);
        token.cancel();
        assert!(queued.into_parts_if_live().is_none());
        drained(&service);
        service.shutdown();
    }
    #[test]
    fn similarity_dispatch_retired_owner_drops_queued_projection() {
        let (_fixture, service, source, snapshot) = setup();
        submit(
            &service,
            request(&source, snapshot.committed_revision, 12),
            SimilarityCancellation::new(),
        );
        let queued = delivery(&service, 12);
        service.shutdown();
        assert!(queued.into_parts_if_live().is_none());
    }
    #[test]
    fn similarity_dispatch_stale_revision_and_missing_anchor_are_typed_failures() {
        let (_fixture, service, source, snapshot) = setup();
        submit(
            &service,
            request(&source, snapshot.committed_revision + 1, 13),
            SimilarityCancellation::new(),
        );
        assert!(matches!(
            delivery(&service, 13).into_event_if_live().unwrap(),
            CatalogEvent::SimilarityCompleted {
                result: Err(SimilarityDispatchFailure::StaleSource),
                ..
            }
        ));
        let missing = SourceSimilarityRequest::new(
            source.clone(),
            1,
            snapshot.committed_revision,
            14,
            "missing.wav".into(),
            &["same.wav".into()],
            SimilarityAspect::Spectrum,
        )
        .unwrap();
        submit(&service, missing, SimilarityCancellation::new());
        assert!(matches!(
            delivery(&service, 14).into_event_if_live().unwrap(),
            CatalogEvent::SimilarityCompleted {
                result: Err(SimilarityDispatchFailure::AnchorUnavailable),
                ..
            }
        ));
        drained(&service);
        service.shutdown();
    }
    #[test]
    fn similarity_dispatch_precancelled_request_releases_unpublished_credits() {
        let (_fixture, service, source, snapshot) = setup();
        let token = SimilarityCancellation::new();
        token.cancel();
        submit(
            &service,
            request(&source, snapshot.committed_revision, 15),
            token,
        );
        wait_for_tag_page_input_release(&service);
        wait_for_tag_read_payload(&service, 0);
        assert!(service.try_recv_delivery().is_err());
        service.shutdown();
    }
    #[test]
    fn similarity_dispatch_constructor_rejects_invalid_and_over_budget_sets() {
        let source = SourceId::from_string("bounds");
        let make = |anchor: &str, paths: &[String], generation| {
            SourceSimilarityRequest::new(
                source.clone(),
                1,
                1,
                generation,
                anchor.into(),
                paths,
                SimilarityAspect::Spectrum,
            )
        };
        for path in [
            "",
            "../escape",
            "a//b",
            "a/./b",
            "/absolute",
            "a\\b",
            "a\0b",
        ] {
            assert_eq!(
                make(path, &[], 1),
                Err(SimilarityDispatchFailure::InvalidInput)
            );
        }
        assert!(make("anchor", &[], 0).is_err());
        assert!(make("anchor", &["anchor".into()], 1).is_err());
        assert!(make("anchor", &["same".into(), "same".into()], 1).is_err());
        assert!(
            make(
                "anchor",
                &(0..65).map(|i| format!("{i}.wav")).collect::<Vec<_>>(),
                1
            )
            .is_err()
        );
        assert!(make(&"a".repeat(4097), &[], 1).is_err());
        assert!(
            make(
                "anchor",
                &(0..17)
                    .map(|i| format!("{i}/{}", "x".repeat(3990)))
                    .collect::<Vec<_>>(),
                1
            )
            .is_err()
        );
        assert!(
            make(
                "anchor",
                &(0..64).map(|i| format!("{i}.wav")).collect::<Vec<_>>(),
                1
            )
            .is_ok()
        );
    }
    #[test]
    fn similarity_dispatch_full_and_closed_admission_do_not_leak_credits() {
        let fixture = Fixture::new("similarity-dispatch-full");
        let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
        let pause = gate.pause_next();
        for generation in 1..=32 {
            submit(
                &service,
                request(&SourceId::from_string("unregistered"), 1, generation),
                SimilarityCancellation::new(),
            );
        }
        assert_eq!(
            admission_state(&service.admission, |state| state.tag_read_payload_bytes),
            32 * crate::similarity_dispatch::MAX_RESULT_BYTES
        );
        assert_eq!(
            service.submit(CatalogCommand::RequestSimilarity {
                request: request(&SourceId::from_string("unregistered"), 1, 33),
                cancellation: SimilarityCancellation::new(),
            }),
            Err(CatalogSubmitError::Full)
        );
        drop(pause);
        for generation in 1..=32 {
            assert!(matches!(
                delivery(&service, generation).into_event_if_live().unwrap(),
                CatalogEvent::SimilarityCompleted {
                    result: Err(SimilarityDispatchFailure::StaleSource),
                    ..
                }
            ));
        }
        drained(&service);
        service.shutdown();
        assert_eq!(
            service.submit(CatalogCommand::RequestSimilarity {
                request: request(&SourceId::from_string("unregistered"), 1, 1),
                cancellation: SimilarityCancellation::new(),
            }),
            Err(CatalogSubmitError::Closed)
        );
    }
}
