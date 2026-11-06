#[cfg(test)]
mod fixtures {
    use super::super::*;
    use yield_library::sample_sources::{
        SourcePlaymarkGridTempo, db::SourcePlaymarkTempo, readiness::SourceFeatureInputError,
    };

    const FRAMES: u64 = 16_000;
    const SAMPLE_RATE: u32 = 16_000;

    fn write_audio(media: &Path) {
        let mut wav = hound::WavWriter::create(
            media,
            hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..FRAMES {
            wav.write_sample(0_i16).unwrap();
        }
        wav.finalize().unwrap();
    }

    #[test]
    fn authored_playmark_tempo_is_atomic_and_bound_to_the_audited_file_across_reopen() {
        let fixture = Fixture::new("authored-playmark-tempo");
        let path = RootRelativePath::try_from_path("loop.wav".into()).unwrap();
        let media = fixture.root.path().join(path.as_path());
        let frames = FRAMES;
        let sample_rate = SAMPLE_RATE;
        write_audio(&media);
        let original_bytes = fs::read(&media).unwrap();
        let source = SourceId::from_string("authored-playmark-tempo");
        let audit = || {
            let service = CatalogService::start(&fixture.guard).unwrap();
            register_and_audit(&service, fixture.root.path(), &source);
            service.shutdown();
        };
        let open = |generation| {
            let database = SourceDatabase::open_for_catalog_with_profile_guard(
                fixture.root.path(),
                &fixture.guard,
                catalog_profile_child(&source),
            )
            .unwrap();
            SourceWriter::new(database, source.clone(), generation).unwrap()
        };
        audit();
        let writer = open(2);
        let revision = writer.current_revision().unwrap();
        let first = SourcePlaymarkTempo::new(0, frames, frames, sample_rate, 2).unwrap();
        let second = SourcePlaymarkTempo::new(1_000, 13_000, frames, sample_rate, 2).unwrap();
        let grid = SourcePlaymarkGridTempo::parse("123.145").unwrap();
        let new_grid = SourcePlaymarkGridTempo::parse("129.5").unwrap();
        let active = AtomicBool::new(false);
        assert!(SourcePlaymarkTempo::new(0, frames + 1, frames, sample_rate, 2).is_none());
        assert_eq!(writer.read_playmark_tempo(&revision, &path).unwrap(), None);
        assert_eq!(
            writer.read_playmark_grid_tempo(&revision, &path).unwrap(),
            None
        );
        assert!(
            writer
                .compare_and_set_playmark_grid_tempo(&revision, &path, None, Some(grid), &active)
                .unwrap()
        );
        assert!(
            !writer
                .compare_and_set_playmark_grid_tempo(
                    &revision,
                    &path,
                    None,
                    Some(new_grid),
                    &active
                )
                .unwrap()
        );
        assert!(matches!(
            writer.compare_and_set_playmark_grid_tempo(
                &revision,
                &path,
                Some(grid),
                Some(new_grid),
                &AtomicBool::new(true)
            ),
            Err(SourceWriterError::FeatureInput(
                SourceFeatureInputError::Cancelled
            ))
        ));
        assert!(
            writer
                .compare_and_set_playmark_tempo(&revision, &path, None, Some(first), &active)
                .unwrap()
        );
        assert!(
            !writer
                .compare_and_set_playmark_tempo(&revision, &path, None, Some(second), &active)
                .unwrap()
        );
        assert!(matches!(
            writer.compare_and_set_playmark_tempo(
                &revision,
                &path,
                Some(first),
                Some(second),
                &AtomicBool::new(true)
            ),
            Err(SourceWriterError::FeatureInput(
                SourceFeatureInputError::Cancelled
            ))
        ));
        assert_eq!(writer.current_revision().unwrap(), revision);
        assert_eq!(fs::read(&media).unwrap(), original_bytes);
        drop(writer);

        let writer = open(3);
        let current = writer.current_revision().unwrap();
        assert_eq!(
            writer.read_playmark_grid_tempo(&current, &path).unwrap(),
            Some(grid)
        );
        assert!(
            writer
                .compare_and_set_playmark_grid_tempo(
                    &current,
                    &path,
                    Some(grid),
                    Some(new_grid),
                    &active
                )
                .unwrap()
        );
        assert_eq!(
            writer.read_playmark_tempo(&current, &path).unwrap(),
            Some(first)
        );
        assert_eq!(first.bpm(), 120.0);
        assert!(
            writer
                .compare_and_set_playmark_tempo(&current, &path, Some(first), Some(second), &active)
                .unwrap()
        );
        assert!(
            writer
                .compare_and_set_playmark_tempo(&current, &path, Some(second), Some(first), &active)
                .unwrap()
        );
        assert_eq!(
            writer.read_playmark_tempo(&current, &path).unwrap(),
            Some(first)
        );
        assert!(
            writer
                .compare_and_set_playmark_tempo(&current, &path, Some(first), None, &active)
                .unwrap()
        );
        assert_eq!(writer.read_playmark_tempo(&current, &path).unwrap(), None);
        assert_eq!(
            writer.read_playmark_grid_tempo(&current, &path).unwrap(),
            Some(new_grid)
        );
        assert!(
            writer
                .compare_and_set_playmark_grid_tempo(&current, &path, Some(new_grid), None, &active)
                .unwrap()
        );
        assert_eq!(
            writer.read_playmark_grid_tempo(&current, &path).unwrap(),
            None
        );
        assert!(
            writer
                .compare_and_set_playmark_grid_tempo(&current, &path, None, Some(grid), &active)
                .unwrap()
        );
        assert!(
            writer
                .compare_and_set_playmark_tempo(&current, &path, None, Some(first), &active)
                .unwrap()
        );
        assert!(writer.read_playmark_tempo(&revision, &path).is_err());

        let displaced = media.with_extension("old");
        fs::rename(&media, &displaced).unwrap();
        fs::copy(&displaced, &media).unwrap();
        assert!(writer.read_playmark_tempo(&current, &path).is_err());
        assert!(writer.read_playmark_grid_tempo(&current, &path).is_err());
        drop(writer);
        audit();
        let writer = open(4);
        let current = writer.current_revision().unwrap();
        assert_eq!(writer.read_playmark_tempo(&current, &path).unwrap(), None);
        assert_eq!(
            writer.read_playmark_grid_tempo(&current, &path).unwrap(),
            None
        );
        assert_eq!(fs::read(&media).unwrap(), original_bytes);
    }

    #[test]
    fn catalog_grid_tempo_keeps_exact_values_and_retired_commit_receipts() {
        let fixture = Fixture::new("catalog-grid-tempo");
        let path = Path::new("loop.wav");
        write_audio(&fixture.root.path().join(path));
        let source = SourceId::from_string("catalog-grid-tempo");
        let service = CatalogService::start(&fixture.guard).unwrap();
        let revision =
            register_and_audit(&service, fixture.root.path(), &source).committed_revision;
        let tempo = SourcePlaymarkGridTempo::parse("123.145").unwrap();
        let next = SourcePlaymarkGridTempo::parse("129.5").unwrap();
        let submit = |id, expected, operation, cancelled| {
            let request =
                SourcePlaymarkGridTempoRequest::new(&source, 1, expected, id, path, operation)
                    .unwrap();
            service
                .submit(CatalogCommand::PlaymarkGridTempo {
                    request,
                    cancellation: Arc::new(AtomicBool::new(cancelled)),
                })
                .unwrap();
            service
                .recv_delivery_timeout(Duration::from_secs(2))
                .unwrap()
        };
        let unpack = |delivery: CatalogDelivery, id| {
            let CatalogEvent::PlaymarkGridTempoCompleted {
                request,
                publication,
                result,
                ..
            } = delivery.into_event_if_live().unwrap()
            else {
                panic!("expected grid tempo receipt")
            };
            assert_eq!(request.request_id(), id);
            assert_eq!(request.source_id(), &source);
            assert_eq!(request.relative_path(), path);
            (publication, result)
        };
        let read = PlaymarkGridTempoOperation::Read;
        let save = PlaymarkGridTempoOperation::Replace {
            previous: None,
            next: Some(tempo),
        };
        assert_eq!(
            unpack(submit(1, revision, read, false), 1),
            (CatalogPublicationStatus::Current, Ok(None))
        );
        assert_eq!(
            unpack(submit(2, revision, save, false), 2),
            (CatalogPublicationStatus::Current, Ok(Some(tempo)))
        );
        assert_eq!(
            unpack(submit(3, revision, save, false), 3).1,
            Err(PlaymarkTempoFailure::Conflict)
        );
        assert_eq!(
            unpack(submit(4, revision + 1, read, false), 4).1,
            Err(PlaymarkTempoFailure::StaleSource)
        );
        let replace = PlaymarkGridTempoOperation::Replace {
            previous: Some(tempo),
            next: Some(next),
        };
        assert_eq!(
            unpack(submit(5, revision, replace, true), 5).1,
            Err(PlaymarkTempoFailure::Cancelled)
        );
        assert_eq!(
            unpack(submit(6, revision, read, false), 6).1,
            Ok(Some(tempo))
        );
        let committed = submit(7, revision, replace, false);
        service
            .submit(CatalogCommand::Remove {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive_until(&service, |event| {
            matches!(event, CatalogEvent::Removed { .. })
        });
        assert_eq!(
            unpack(committed, 7),
            (CatalogPublicationStatus::NonProjectable, Ok(Some(next)))
        );
        wait_for_tag_read_payload(&service, 0);
        wait_for_tag_page_input_release(&service);
        service.shutdown();
    }

    #[test]
    fn catalog_tempo_requests_settle_conflicts_and_preserve_retired_commit_receipts() {
        let fixture = Fixture::new("catalog-playmark-tempo");
        let path = Path::new("loop.wav");
        write_audio(&fixture.root.path().join(path));
        let source = SourceId::from_string("catalog-playmark-tempo");
        let service = CatalogService::start(&fixture.guard).unwrap();
        let revision =
            register_and_audit(&service, fixture.root.path(), &source).committed_revision;
        let tempo = SourcePlaymarkTempo::new(1_000, 13_000, FRAMES, SAMPLE_RATE, 2).unwrap();
        let submit = |id, expected, operation, cancellation| {
            let request =
                SourcePlaymarkTempoRequest::new(&source, 1, expected, id, path, operation).unwrap();
            service
                .submit(CatalogCommand::PlaymarkTempo {
                    request,
                    cancellation,
                })
                .unwrap();
            service
                .recv_delivery_timeout(Duration::from_secs(2))
                .unwrap()
        };
        let unpack = |delivery: CatalogDelivery, id| {
            let CatalogEvent::PlaymarkTempoCompleted {
                request,
                publication,
                result,
                ..
            } = delivery.into_event_if_live().unwrap()
            else {
                panic!("expected tempo completion")
            };
            assert_eq!(request.request_id(), id);
            assert_eq!(request.source_id(), &source);
            assert_eq!(request.relative_path(), path);
            (publication, result)
        };
        let active = || Arc::new(AtomicBool::new(false));
        let read = PlaymarkTempoOperation::Read;
        let save = PlaymarkTempoOperation::Replace {
            previous: None,
            next: Some(tempo),
        };
        assert_eq!(
            unpack(submit(1, revision, read, active()), 1),
            (CatalogPublicationStatus::Current, Ok(None))
        );
        let token = active();
        let committed = submit(2, revision, save, token.clone());
        token.store(true, Ordering::Release);
        assert_eq!(
            unpack(committed, 2),
            (CatalogPublicationStatus::NonProjectable, Ok(Some(tempo)))
        );
        assert_eq!(
            unpack(submit(3, revision, read, active()), 3),
            (CatalogPublicationStatus::Current, Ok(Some(tempo)))
        );
        assert_eq!(
            unpack(submit(4, revision, save, active()), 4).1,
            Err(PlaymarkTempoFailure::Conflict)
        );
        assert_eq!(
            unpack(submit(5, revision + 1, read, active()), 5).1,
            Err(PlaymarkTempoFailure::StaleSource)
        );
        let clear = PlaymarkTempoOperation::Replace {
            previous: Some(tempo),
            next: None,
        };
        assert_eq!(
            unpack(
                submit(6, revision, clear, Arc::new(AtomicBool::new(true))),
                6
            )
            .1,
            Err(PlaymarkTempoFailure::Cancelled)
        );
        assert_eq!(
            unpack(submit(7, revision, clear, active()), 7),
            (CatalogPublicationStatus::Current, Ok(None))
        );
        assert_eq!(unpack(submit(8, revision, read, active()), 8).1, Ok(None));
        let committed = submit(9, revision, save, active());
        service
            .submit(CatalogCommand::Remove {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive_until(&service, |event| {
            matches!(event, CatalogEvent::Removed { .. })
        });
        assert_eq!(
            unpack(committed, 9),
            (CatalogPublicationStatus::NonProjectable, Ok(Some(tempo))),
        );
        assert!(matches!(
            SourcePlaymarkTempoRequest::new(
                &source,
                1,
                revision,
                10,
                Path::new("../loop.wav"),
                read
            ),
            Err(PlaymarkTempoFailure::InvalidInput),
        ));
        wait_for_tag_read_payload(&service, 0);
        wait_for_tag_page_input_release(&service);
        service.shutdown();
    }
}
