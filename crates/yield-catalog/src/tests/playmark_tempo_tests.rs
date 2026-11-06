#[cfg(test)]
mod fixtures {
    use super::super::*;
    use yield_library::sample_sources::{
        db::SourcePlaymarkTempo, readiness::SourceFeatureInputError,
    };

    #[test]
    fn authored_playmark_tempo_is_atomic_and_bound_to_the_audited_file_across_reopen() {
        let fixture = Fixture::new("authored-playmark-tempo");
        let path = RootRelativePath::try_from_path("loop.wav".into()).unwrap();
        let media = fixture.root.path().join(path.as_path());
        let frames = 16_000;
        let sample_rate = 16_000;
        let mut wav = hound::WavWriter::create(
            &media,
            hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..frames {
            wav.write_sample(0_i16).unwrap();
        }
        wav.finalize().unwrap();
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
        let active = AtomicBool::new(false);
        assert!(SourcePlaymarkTempo::new(0, frames + 1, frames, sample_rate, 2).is_none());
        assert_eq!(writer.read_playmark_tempo(&revision, &path).unwrap(), None);
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
        drop(writer);
        audit();
        let writer = open(4);
        let current = writer.current_revision().unwrap();
        assert_eq!(writer.read_playmark_tempo(&current, &path).unwrap(), None);
        assert_eq!(fs::read(&media).unwrap(), original_bytes);
    }
}
