use super::*;
use crate::browser::transport::PlaybackIntent;
use reson::{PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest, PlaybackRuntimeStreamPolicy};
use std::path::PathBuf;

fn audition(path: PathBuf) -> ResolvedAudition {
    ResolvedAudition {
        intent: PlaybackIntent {
            start: 0.0,
            region: None,
            looped: true,
            volume: 1.0,
            path: path.clone(),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        },
        request: PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile { path },
            mode: PlaybackRuntimeMode::Looped {
                start: 0.0,
                end: 1.0,
                offset: 0.25,
            },
            stream_policy: PlaybackRuntimeStreamPolicy::full(),
            volume: 1.0,
            anti_click_enabled: true,
            anti_click_fade_ms: 3.0,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: None,
            metronome: None,
        },
        start: 0.0,
        metronome_grid: None,
        metronome_beat_count: None,
        tempo_label: None,
    }
}

#[gpui::test]
fn target_tempo_settings_defaults_to_raw_and_invalid_input_stays_raw(
    cx: &mut gpui::TestAppContext,
) {
    cx.set_global(crate::theme::ActiveTheme::default());
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    browser.update(cx, |view, cx| {
        view.theme_settings_open = true;
        let raw = route(view, audition("source.wav".into()), cx).unwrap();
        assert_eq!(raw.tempo_label, None);
        assert!(matches!(
            raw.request.source,
            PlaybackRuntimeSource::WavFile { .. }
        ));
        assert!(!view.tempo_audition.is_preparing());

        view.tempo_audition.input.update(cx, |input, cx| {
            input.set_content("invalid", cx);
        });
        let raw = route(view, audition("source.wav".into()), cx).unwrap();
        assert_eq!(
            raw.tempo_label,
            Some(TempoPlaybackLabel::Raw("invalid target BPM"))
        );
        assert!(!view.tempo_audition.is_preparing());
        cx.notify();
    });
    cx.refresh().unwrap();
    assert!(cx.debug_bounds(CONTROL).is_some());
}

#[gpui::test]
fn explicit_target_builds_and_prepares_a_fenced_wav_job(cx: &mut gpui::TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 12_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..1_201 {
        writer.write_sample(2_000_i16).unwrap();
    }
    writer.finalize().unwrap();
    let preview = yield_media::load_waveform_preview(&path).unwrap();

    let browser = cx.new(Browser::new);
    let job = browser.update(cx, |view, _| {
        view.waveform = Some(preview);
        let audition = audition(path.clone());
        assert!(matches!(
            make_job(view, &audition, 150.0),
            Err("source BPM unavailable")
        ));
        view.bpm_snap_entry.tempo = SourcePlaymarkGridTempo::parse("100");
        let job = make_job(view, &audition, 150.0).unwrap();
        assert_eq!(job.source_span, 0..1_201);
        assert_eq!(job.loop_start, 300);
        job
    });
    assert!(prepare(job, &AtomicBool::new(false)).is_ok());
    assert_eq!(
        playing_status(Some(TempoPlaybackLabel::Stretched(150.0)), Some(0.25)),
        "Playing 25% · 150.00 BPM"
    );
}

#[gpui::test]
fn expired_worker_cannot_reenter_after_stop_or_admit_a_second_job(cx: &mut gpui::TestAppContext) {
    let browser = cx.new(Browser::new);
    browser.update(cx, |view, cx| {
        let cancel = Arc::new(AtomicBool::new(false));
        view.tempo_audition.active = Some(Arc::clone(&cancel));
        view.tempo_audition.pending = Some(Pending {
            playback_epoch: view.playback_epoch,
            waveform_generation: view.waveform_generation,
            output_open_generation: view.audio_output_open_generation,
            revision: WaveformFileRevision {
                filesystem_identity: "fixture".into(),
                change_marker: "fixture".into(),
                byte_len: 44,
                modified_time: None,
            },
            audition: audition("source.wav".into()),
            target_bpm: 150.0,
        });

        let wrong_worker = Arc::new(AtomicBool::new(false));
        complete(view, &wrong_worker, Some(Err("late result")), cx);
        assert!(view.tempo_audition.is_preparing());
        assert!(!cancel.load(Ordering::Acquire));

        complete(view, &cancel, None, cx);
        assert!(!view.tempo_audition.is_preparing());
        assert!(cancel.load(Ordering::Acquire));
        assert!(view.tempo_audition.active.is_some());
        view.stop_playback();

        complete(view, &cancel, Some(Err("late result")), cx);
        assert!(view.tempo_audition.active.is_none());
        assert!(!view.tempo_audition.is_preparing());
        assert_eq!(view.playback_status, "Stopped");
    });
}
