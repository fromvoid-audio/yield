use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::async_decode::AsyncDecodeDiagnostics;
use crate::source::{BandLimitedOutputAdapter, RenderedSourcePosition, StereoOutputPair};
use crate::telemetry;
use crate::{OutputAdapter, Source};
use tracing::warn;

#[cfg(test)]
use super::super::DEFAULT_ANTI_CLIP_FADE;
#[cfg(test)]
use super::super::fade::{EdgeFade, fade_duration};
use super::super::fade::{FadeOutHandle, FadeOutOnRequest, fade_frames_for_duration};
use super::{AudioPlaybackSource, AudioPlayer, PlaybackRuntimeReplacePolicy};
#[cfg(test)]
use crate::mixer::{decoder_from_bytes, map_seek_error};

pub(super) enum PlaybackRateConversion {
    Linear,
    BandLimited,
}

type RoutedSource = (Box<dyn Source + Send>, Option<RenderedSourcePosition>);

impl PlaybackRateConversion {
    fn adapt<S: Source + 'static>(
        self,
        source: S,
        target_rate: u32,
        target_channels: u16,
        output_pair: StereoOutputPair,
    ) -> Result<RoutedSource, String> {
        let source_rate = source.sample_rate().max(1);
        if matches!(self, Self::BandLimited) && source_rate != target_rate {
            let adapter = BandLimitedOutputAdapter::new(
                source,
                target_rate,
                target_channels,
                output_pair,
            )
            .ok_or_else(|| {
                format!(
                    "Unsupported audition rate conversion from {source_rate} Hz to {target_rate} Hz"
                )
            })?;
            let position = adapter.rendered_position();
            Ok((Box::new(adapter), Some(position)))
        } else {
            Ok((
                Box::new(OutputAdapter::with_output_pair(
                    source,
                    target_rate,
                    target_channels,
                    output_pair,
                )),
                None,
            ))
        }
    }
}

#[cfg(test)]
mod rate_conversion_tests {
    use super::*;
    use crate::SamplesBuffer;

    #[test]
    fn prepared_rate_route_filters_aliases_and_preserves_output_pair_and_eof() {
        let input = || {
            let samples = (0..1024)
                .flat_map(|frame| {
                    let sign = if frame % 2 == 0 { 1.0 } else { -1.0 };
                    [sign, -sign]
                })
                .collect::<Vec<_>>();
            SamplesBuffer::new(2, 96_000, samples)
        };
        let pair = StereoOutputPair::new(1);
        let (mut prepared, prepared_position) = PlaybackRateConversion::BandLimited
            .adapt(input(), 48_000, 4, pair)
            .unwrap();
        let (mut regular, regular_position) = PlaybackRateConversion::Linear
            .adapt(input(), 48_000, 4, pair)
            .unwrap();
        assert_eq!((prepared.sample_rate(), prepared.channels()), (48_000, 4));
        assert!(prepared_position.is_some());
        assert!(regular_position.is_none());

        let counts = crate::test_support::count_thread_allocations_and_deallocations(|| {
            for frame in 0..512 {
                for stream in [&mut prepared, &mut regular] {
                    assert_eq!(stream.next(), Some(0.0));
                    assert_eq!(stream.next(), Some(0.0));
                }
                let left = prepared.next().unwrap();
                assert_eq!(prepared.next(), Some(-left));
                if (96..416).contains(&frame) {
                    assert!(left.abs() < 1e-4, "aliased tone at frame {frame}: {left}");
                }
                assert_eq!(regular.next(), Some(1.0));
                assert_eq!(regular.next(), Some(-1.0));
            }
            assert_eq!(prepared.next(), None);
            assert_eq!(regular.next(), None);
        });
        assert_eq!(counts, (0, 0));
    }

    #[test]
    fn prepared_rate_route_bypasses_matching_rates_and_folds_multichannel_to_mono() {
        let samples = SamplesBuffer::new(2, 48_000, vec![-0.0, f32::MAX]);
        let (mut exact, position) = PlaybackRateConversion::BandLimited
            .adapt(samples, 48_000, 4, StereoOutputPair::new(1))
            .unwrap();
        assert!(position.is_none());
        for expected in [0.0_f32, 0.0, -0.0, f32::MAX] {
            assert_eq!(exact.next().map(f32::to_bits), Some(expected.to_bits()));
        }
        assert_eq!(exact.next(), None);

        let samples = SamplesBuffer::new(3, 44_100, vec![0.25, 0.5, 0.75]);
        let mono = PlaybackRateConversion::BandLimited
            .adapt(samples, 96_000, 1, StereoOutputPair::default())
            .unwrap()
            .0
            .collect::<Vec<_>>();
        assert_eq!(mono.len(), 3);
        assert!(mono.into_iter().all(|value| (value - 0.5).abs() < 1e-6));
    }
}

impl AudioPlayer {
    pub(super) fn effective_volume(&self) -> f32 {
        let volume = self.volume * self.playback_gain;
        if volume.is_finite() {
            volume.max(0.0)
        } else {
            0.0
        }
    }

    pub(super) fn reset_playback_state(&mut self) {
        self.started_at = None;
        self.play_span = None;
        self.play_span_frames = None;
        self.looping = false;
        self.active_playback_span = None;
        self.active_loop_mode = None;
        self.active_metronome = None;
        self.rendered_source_position = None;
        self.loop_offset = None;
        self.loop_offset_frames = None;
        #[cfg(test)]
        {
            self.elapsed_override = None;
        }
    }

    pub(super) fn build_sink_with_fade<S: Source + Send + 'static>(
        &mut self,
        source: S,
        replace_policy: PlaybackRuntimeReplacePolicy,
        diagnostics: Option<Arc<AsyncDecodeDiagnostics>>,
        rate_conversion: PlaybackRateConversion,
    ) -> Result<(FadeOutHandle, (u32, u16)), String> {
        let started_at = telemetry::playback_telemetry_enabled().then(Instant::now);
        let _volume = self.effective_volume();
        let target_sample_rate = self.output.sample_rate.max(1);
        let target_channels = self.output.channel_count.max(1);
        let input_sample_rate = source.sample_rate();
        let input_channels = source.channels();
        let output_pair = StereoOutputPair::default();
        let adapted = input_sample_rate != target_sample_rate
            || input_channels != target_channels
            || !output_pair.can_bypass(input_channels.max(1) as usize, target_channels as usize);
        let (source, rendered_source_position): RoutedSource = if !adapted {
            (Box::new(source), None)
        } else {
            rate_conversion.adapt(source, target_sample_rate, target_channels, output_pair)?
        };
        let format = (source.sample_rate(), source.channels());
        let handle = FadeOutHandle::new();

        let append_started_at = telemetry::playback_telemetry_enabled().then(Instant::now);
        let append_result = match replace_policy {
            PlaybackRuntimeReplacePolicy::FadeOutPrevious => self
                .stream
                .append_source(FadeOutOnRequest::new(source, handle.clone()), 1.0),
            PlaybackRuntimeReplacePolicy::ClearPrevious => {
                self.stream.append_source_replacing_previous(
                    FadeOutOnRequest::new(source, handle.clone()),
                    1.0,
                )
            }
        };
        append_result.map_err(|err| {
            warn!("Failed to append audio source: {err}");
            err
        })?;
        if let Some(diagnostics) = diagnostics {
            diagnostics.mark_admitted();
        }
        self.rendered_source_position = rendered_source_position;
        if let Some(append_started_at) = append_started_at {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "append_source",
                input_sample_rate,
                input_channels,
                target_sample_rate,
                target_channels,
                adapted,
                elapsed_ms = telemetry::elapsed_ms(append_started_at.elapsed()),
                "Audio player stage"
            );
        }
        self.active_sources = match replace_policy {
            PlaybackRuntimeReplacePolicy::FadeOutPrevious => self.active_sources.saturating_add(1),
            PlaybackRuntimeReplacePolicy::ClearPrevious => 1,
        };
        if let Some(started_at) = started_at {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "build_sink_with_fade",
                input_sample_rate,
                input_channels,
                target_sample_rate,
                target_channels,
                adapted,
                elapsed_ms = telemetry::elapsed_ms(started_at.elapsed()),
                "Audio player stage"
            );
        }

        Ok((handle, format))
    }

    pub(super) fn prepare_previous_sink_for_new_source(
        &mut self,
        fade: Duration,
        replace_policy: PlaybackRuntimeReplacePolicy,
    ) {
        match replace_policy {
            PlaybackRuntimeReplacePolicy::FadeOutPrevious => self.fade_out_current_sink(fade),
            PlaybackRuntimeReplacePolicy::ClearPrevious => {
                self.fade_out = None;
                self.sink_format = None;
                self.active_sources = 0;
            }
        }
    }

    /// Create a monitor sink that taps the current output stream state.
    pub fn create_monitor_sink(&self, volume: f32) -> crate::output::MonitorSink {
        self.stream.monitor_sink(volume)
    }

    pub(super) fn elapsed_since(&self, started_at: Instant) -> Duration {
        #[cfg(test)]
        if let Some(override_elapsed) = self.elapsed_override {
            return override_elapsed;
        }
        started_at.elapsed()
    }

    pub(super) fn audio_source(&self) -> Result<AudioPlaybackSource, String> {
        self.current_audio
            .as_ref()
            .cloned()
            .ok_or_else(|| "Load a .wav file first".to_string())
    }

    pub(super) fn anti_clip_fade(&self) -> Duration {
        if self.anti_clip_enabled {
            self.anti_clip_fade
        } else {
            Duration::ZERO
        }
    }

    pub(super) fn normalized_span(&self, start: f64, end: f64) -> Result<(f32, f32, f32), String> {
        let duration = self
            .track_duration
            .ok_or_else(|| "Load a .wav file first".to_string())?;
        if duration <= 0.0 {
            return Err("Load a .wav file first".into());
        }
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let duration64 = f64::from(duration);
        let clamped_start = start.clamp(0.0, 1.0) * duration64;
        let clamped_end = end.clamp(0.0, 1.0) * duration64;
        let mut bounded_start = clamped_start.min(duration64) as f32;
        let mut bounded_end = clamped_end.min(duration64) as f32;
        let min_span = self.min_span_seconds.unwrap_or(0.01);
        if bounded_end <= bounded_start {
            bounded_end = (bounded_start + min_span).min(duration);
            if bounded_end <= bounded_start {
                bounded_start = (duration - min_span).max(0.0);
                bounded_end = duration.max(bounded_start + 0.001);
            }
        }
        Ok((bounded_start, bounded_end, duration))
    }

    pub(super) fn fade_out_current_sink(&mut self, fade: Duration) {
        if self.active_sources == 0 {
            return;
        }
        let handle = self.fade_out.take();
        let format = self.sink_format.take();
        self.active_sources = 0;

        let Some(handle) = handle else {
            if self.stream.clear_sources().is_err() {
                warn!("Failed to clear audio sources: output stream unavailable");
            }
            return;
        };
        let Some((sample_rate, _channels)) = format else {
            if self.stream.clear_sources().is_err() {
                warn!("Failed to clear audio sources: output stream unavailable");
            }
            return;
        };
        if fade.is_zero() {
            if self.stream.clear_sources().is_err() {
                warn!("Failed to clear audio sources: output stream unavailable");
            }
            return;
        }
        let fade_frames = fade_frames_for_duration(sample_rate, fade);
        handle.request_fade_out_frames(fade_frames);
    }

    #[cfg(test)]
    pub(crate) fn span_sample_count(
        bytes: Arc<[u8]>,
        start_seconds: f32,
        end_seconds: f32,
    ) -> Result<(usize, u32, u16), String> {
        let mut source = decoder_from_bytes(bytes)?;
        source
            .try_seek(Duration::from_secs_f32(start_seconds))
            .map_err(map_seek_error)?;
        let span_length = (end_seconds - start_seconds).max(0.001);
        let fade = fade_duration(span_length, DEFAULT_ANTI_CLIP_FADE);
        let limited = source
            .fade_in(fade)
            .take_duration(Duration::from_secs_f32(span_length))
            .buffered();
        let mut faded = EdgeFade::new(limited, fade);
        let sample_rate = faded.sample_rate();
        let channels = faded.channels();
        let mut count = 0usize;
        while faded.next().is_some() {
            count = count.saturating_add(1);
        }
        Ok((count, sample_rate, channels))
    }
}
