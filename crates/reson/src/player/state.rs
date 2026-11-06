use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::Source;
use crate::telemetry;
use yield_aiff::AiffPcmReader;

use super::super::DEFAULT_ANTI_CLIP_FADE;
use super::super::output::{AudioOutputConfig, ResolvedOutput, open_output_stream};
use super::super::timebase::seconds_to_frames_round;

use super::{
    AudioPlaybackSource, AudioPlayer, EditFadeHandle, EditFadeRange, PlaybackRuntimeReplacePolicy,
    PlaybackRuntimeStreamPolicy,
};

pub(super) fn open_aiff_file_for_playback(path: &Path) -> Result<AiffPcmReader, String> {
    let reader = AiffPcmReader::open(path).map_err(|error| error.to_string())?;
    let sample_rate = reader.sample_rate();
    let channels = reader.channels();
    // Bound decoder-ring sizing before constructing a playback source.
    if sample_rate > 384_000
        || channels > 32
        || u64::from(sample_rate) * u64::from(channels) > 384_000 * 8
    {
        return Err(format!(
            "{} AIFF layout exceeds playback limits ({sample_rate} Hz, {channels} channels)",
            path.display()
        ));
    }
    Ok(reader)
}

impl AudioPlayer {
    /// Create a new audio player using the default output device.
    pub fn new() -> Result<Self, String> {
        Self::from_config(&AudioOutputConfig::default())
    }

    /// Create a new audio player honoring the requested output configuration.
    pub fn from_config(config: &AudioOutputConfig) -> Result<Self, String> {
        let outcome = open_output_stream(config).map_err(|err| err.to_string())?;
        Ok(Self::from_open_stream_outcome(outcome))
    }

    #[cfg(test)]
    pub(crate) fn from_native_test_device(
        device: cpal::Device,
        host_id: String,
        config: &AudioOutputConfig,
        required_channels: u16,
    ) -> Result<Self, String> {
        let outcome = crate::output::open_output_stream_on_test_device(
            device,
            host_id,
            config,
            required_channels,
        )?;
        Ok(Self::from_open_stream_outcome(outcome))
    }

    fn from_open_stream_outcome(outcome: crate::output::OpenStreamOutcome) -> Self {
        Self {
            stream: outcome.stream,
            edit_fade_handle: EditFadeHandle::new(),
            active_sources: 0,
            fade_out: None,
            sink_format: None,
            current_audio: None,
            playback_samples: None,
            track_duration: None,
            track_total_frames: None,
            track_channels: None,
            sample_rate: None,
            started_at: None,
            play_span: None,
            play_span_frames: None,
            looping: false,
            active_playback_span: None,
            active_loop_mode: None,
            active_metronome: None,
            metronome_mix_override: Default::default(),
            loop_offset: None,
            loop_offset_frames: None,
            volume: 1.0,
            playback_gain: 1.0,
            anti_clip_enabled: true,
            anti_clip_fade: DEFAULT_ANTI_CLIP_FADE,
            min_span_seconds: None,
            stream_policy: PlaybackRuntimeStreamPolicy::default(),
            output: outcome.resolved,
            #[cfg(test)]
            elapsed_override: None,
        }
    }

    /// Store audio bytes and duration for later playback.
    pub fn set_audio(&mut self, data: impl Into<Arc<[u8]>>, duration: f32) {
        use super::super::mixer::{
            decoder_duration, decoder_sample_rate, wav_header_duration, wav_spec_from_bytes,
        };
        let audio: Arc<[u8]> = data.into();
        let provided = duration.max(0.0);
        let fallback = wav_header_duration(&audio)
            .or_else(|| decoder_duration(&audio))
            .unwrap_or(0.0);

        let wav_spec = wav_spec_from_bytes(&audio);
        let sample_rate = wav_spec
            .map(|(_, rate, _)| rate)
            .or_else(|| decoder_sample_rate(&audio));
        let channels = wav_spec.map(|(_, _, channels)| channels);
        let chosen = if provided > 0.0 { provided } else { fallback };
        self.track_duration = Some(chosen);
        self.track_total_frames = sample_rate
            .map(|rate| seconds_to_frames_round(chosen, rate).max(1))
            .filter(|frames| *frames > 0);
        self.track_channels = channels;
        self.sample_rate = sample_rate;
        self.current_audio = Some(AudioPlaybackSource::Bytes(audio));
        self.playback_samples = None;
        self.reset_playback_state();
    }

    /// Store audio bytes with caller-provided timing metadata.
    ///
    /// This avoids opening a decoder on latency-sensitive UI paths when the
    /// waveform loader has already established the file duration and sample
    /// rate.
    pub fn set_audio_with_metadata(
        &mut self,
        data: impl Into<Arc<[u8]>>,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    ) {
        let audio: Arc<[u8]> = data.into();
        let duration = duration.max(0.0);
        let sample_rate = sample_rate.max(1);
        self.track_duration = Some(duration);
        self.track_total_frames = Some(seconds_to_frames_round(duration, sample_rate).max(1));
        self.track_channels = Some(channels.clamp(1, u16::MAX as usize) as u16);
        self.sample_rate = Some(sample_rate);
        self.current_audio = Some(AudioPlaybackSource::Bytes(audio));
        self.playback_samples = None;
        self.reset_playback_state();
        if telemetry::playback_telemetry_enabled() {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "set_audio_with_metadata",
                source_kind = "bytes",
                byte_len = self.current_audio.as_ref().and_then(|source| match source {
                    AudioPlaybackSource::Bytes(bytes) => Some(bytes.len()),
                    AudioPlaybackSource::File(_) => None,
                    AudioPlaybackSource::InterleavedF32File { .. } => None,
                }).unwrap_or(0),
                duration_ms = duration as f64 * 1_000.0,
                sample_rate,
                channels = channels.clamp(1, u16::MAX as usize),
                "Audio player stage"
            );
        }
    }

    /// Store an audio file path with caller-provided timing metadata.
    pub fn set_audio_file_with_metadata(
        &mut self,
        path: impl Into<PathBuf>,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    ) {
        let duration = duration.max(0.0);
        let sample_rate = sample_rate.max(1);
        self.track_duration = Some(duration);
        self.track_total_frames = Some(seconds_to_frames_round(duration, sample_rate).max(1));
        self.track_channels = Some(channels.clamp(1, u16::MAX as usize) as u16);
        self.sample_rate = Some(sample_rate);
        self.current_audio = Some(AudioPlaybackSource::File(path.into()));
        self.playback_samples = None;
        self.reset_playback_state();
        if telemetry::playback_telemetry_enabled() {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "set_audio_file_with_metadata",
                source_kind = "file",
                duration_ms = duration as f64 * 1_000.0,
                sample_rate,
                channels = channels.clamp(1, u16::MAX as usize),
                "Audio player stage"
            );
        }
    }

    /// Probe a WAV file and store it as a lazy file-backed playback source.
    ///
    /// Playback runtime callers execute this method on the dedicated runtime
    /// thread, keeping file access and header parsing off latency-sensitive UI
    /// paths.
    pub fn set_wav_file(&mut self, path: impl Into<PathBuf>) -> Result<(), String> {
        let path = path.into();
        let (duration, sample_rate, channels) = wav_file_metadata(&path)?;
        self.set_audio_file_with_metadata(path, duration, sample_rate, channels);
        Ok(())
    }

    pub(super) fn set_aiff_file(&mut self, path: PathBuf) -> Result<(), String> {
        if !yield_aiff::is_aiff_path(&path) {
            return Err(format!(
                "AIFF playback requires an .aiff or .aif path: {}",
                path.display()
            ));
        }
        let reader = open_aiff_file_for_playback(&path)?;
        let duration = (reader.frames() as f64 / f64::from(reader.sample_rate())) as f32;
        self.set_audio_file_with_metadata(
            path,
            duration,
            reader.sample_rate(),
            usize::from(reader.channels()),
        );
        self.track_total_frames = Some(reader.frames());
        Ok(())
    }

    /// Store a raw little-endian interleaved f32 sample file with caller-provided timing metadata.
    pub fn set_interleaved_f32_file_with_metadata(
        &mut self,
        path: impl Into<PathBuf>,
        sample_count: u64,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    ) {
        let path = path.into();
        let duration = duration.max(0.0);
        let sample_rate = sample_rate.max(1);
        self.track_duration = Some(duration);
        self.track_total_frames = Some(seconds_to_frames_round(duration, sample_rate).max(1));
        self.track_channels = Some(channels.clamp(1, u16::MAX as usize) as u16);
        self.sample_rate = Some(sample_rate);
        self.current_audio = Some(AudioPlaybackSource::InterleavedF32File { path, sample_count });
        self.playback_samples = None;
        self.reset_playback_state();
        if telemetry::playback_telemetry_enabled() {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "set_interleaved_f32_file_with_metadata",
                source_kind = "interleaved_f32_file",
                sample_count,
                duration_ms = duration as f64 * 1_000.0,
                sample_rate,
                channels = channels.clamp(1, u16::MAX as usize),
                "Audio player stage"
            );
        }
    }

    /// Store audio bytes and decoded playback samples with caller-provided
    /// timing metadata.
    pub fn set_audio_samples_with_metadata(
        &mut self,
        data: impl Into<Arc<[u8]>>,
        samples: Arc<[f32]>,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    ) {
        let audio: Arc<[u8]> = data.into();
        let duration = duration.max(0.0);
        let sample_rate = sample_rate.max(1);
        self.track_duration = Some(duration);
        self.track_total_frames = Some(seconds_to_frames_round(duration, sample_rate).max(1));
        self.track_channels = Some(channels.clamp(1, u16::MAX as usize) as u16);
        self.sample_rate = Some(sample_rate);
        self.current_audio = Some(AudioPlaybackSource::Bytes(audio));
        self.playback_samples = Some(samples);
        self.reset_playback_state();
        if telemetry::playback_telemetry_enabled() {
            tracing::info!(
                target: "perf::audio_start",
                module = "reson_player",
                stage = "set_audio_samples_with_metadata",
                source_kind = "samples",
                byte_len = self.current_audio.as_ref().and_then(|source| match source {
                    AudioPlaybackSource::Bytes(bytes) => Some(bytes.len()),
                    AudioPlaybackSource::File(_) => None,
                    AudioPlaybackSource::InterleavedF32File { .. } => None,
                }).unwrap_or(0),
                sample_len = self.playback_samples.as_ref().map(|samples| samples.len()).unwrap_or(0),
                duration_ms = duration as f64 * 1_000.0,
                sample_rate,
                channels = channels.clamp(1, u16::MAX as usize),
                "Audio player stage"
            );
        }
    }

    /// Adjust master output volume for current and future playback.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        let effective = self.effective_volume();
        self.stream.set_volume(effective);
    }

    /// Adjust normalized audition gain for current and future playback.
    pub fn set_playback_gain(&mut self, gain: f32) {
        self.playback_gain = if gain.is_finite() && gain > 0.0 {
            gain
        } else {
            1.0
        };
        let effective = self.effective_volume();
        self.stream.set_volume(effective);
    }

    /// Set the minimum span length (in seconds) enforced for playback ranges.
    pub fn set_min_span_seconds(&mut self, min_span: Option<f32>) {
        self.min_span_seconds = min_span.filter(|value| value.is_finite() && *value > 0.0);
    }

    /// Configure lazy-source buffering for the next playback request.
    pub fn set_stream_policy(&mut self, policy: PlaybackRuntimeStreamPolicy) {
        self.stream_policy = policy;
    }

    /// Configure the anti-click fade used for playback edges.
    pub fn set_anti_clip_settings(&mut self, enabled: bool, fade_ms: f32) {
        (self.anti_clip_enabled, self.anti_clip_fade) = anti_clip_settings(enabled, fade_ms);
    }

    /// Stop any active playback.
    pub fn stop(&mut self) {
        self.fade_out_current_sink(self.anti_clip_fade());
        self.reset_playback_state();
    }

    /// Active output configuration after initialization.
    pub fn output_details(&self) -> &ResolvedOutput {
        &self.output
    }

    #[cfg(test)]
    pub(crate) fn native_callback_sample_count(&self, channel: usize) -> u64 {
        self.stream.callback_probe().sample_count(channel)
    }

    #[cfg(test)]
    pub(crate) fn native_callback_peak(&self, channel: usize) -> f32 {
        self.stream.callback_probe().peak(channel)
    }

    #[cfg(test)]
    pub(crate) fn native_callback_allocation_count(&self) -> u64 {
        self.stream.callback_probe().allocation_count()
    }

    #[cfg(test)]
    pub(crate) fn native_callback_deallocation_count(&self) -> u64 {
        self.stream.callback_probe().deallocation_count()
    }

    #[cfg(test)]
    pub(crate) fn native_callback_non_finite_sample_count(&self) -> u64 {
        self.stream.callback_probe().non_finite_sample_count()
    }

    #[cfg(test)]
    pub(crate) fn native_callback_rejected_contribution_count(&self) -> u64 {
        self.stream.callback_probe().rejected_contribution_count()
    }

    #[cfg(test)]
    pub(crate) fn native_active_source_count(&self) -> usize {
        self.stream.active_source_count()
    }

    #[cfg(test)]
    pub(crate) fn native_stream_teardown_complete(
        &self,
    ) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.stream.teardown_complete()
    }

    /// Update the realtime fade state for edit selections.
    pub fn set_edit_fade_state(&self, range: Option<EditFadeRange>) {
        if let Some(duration) = self.track_duration {
            self.edit_fade_handle.update(range, duration);
        } else {
            self.edit_fade_handle.update(None, 0.0);
        }
    }

    #[cfg(test)]
    pub(crate) fn test_with_state(
        stream: crate::output::CpalAudioStream,
        track_duration: Option<f32>,
        started_at: Option<Instant>,
        play_span: Option<(f32, f32)>,
        looping: bool,
        loop_offset: Option<f32>,
        elapsed_override: Option<Duration>,
    ) -> Self {
        Self {
            stream,
            edit_fade_handle: EditFadeHandle::new(),
            active_sources: 0,
            fade_out: None,
            sink_format: None,
            current_audio: None,
            playback_samples: None,
            track_duration,
            track_total_frames: None,
            track_channels: None,
            sample_rate: None,
            started_at,
            play_span,
            play_span_frames: None,
            looping,
            active_playback_span: None,
            active_loop_mode: None,
            active_metronome: None,
            metronome_mix_override: Default::default(),
            loop_offset,
            loop_offset_frames: None,
            volume: 1.0,
            playback_gain: 1.0,
            anti_clip_enabled: true,
            anti_clip_fade: DEFAULT_ANTI_CLIP_FADE,
            min_span_seconds: None,
            stream_policy: PlaybackRuntimeStreamPolicy::default(),
            output: ResolvedOutput::default(),
            elapsed_override,
        }
    }

    /// Build a looped playing instance for tests that need an active sink.
    #[doc(hidden)]
    pub fn playing_for_tests() -> Option<Self> {
        if !test_audio_output_enabled() {
            return None;
        }

        struct SineWave {
            pos: f32,
            step: f32,
        }
        impl Iterator for SineWave {
            type Item = f32;
            fn next(&mut self) -> Option<Self::Item> {
                let val = self.pos.sin();
                self.pos += self.step;
                Some(val)
            }
        }
        impl Source for SineWave {
            fn current_frame_len(&self) -> Option<usize> {
                None
            }
            fn channels(&self) -> u16 {
                1
            }
            fn sample_rate(&self) -> u32 {
                44100
            }
            fn total_duration(&self) -> Option<Duration> {
                None
            }
        }

        let mut player = AudioPlayer::new().ok()?;
        let source = SineWave {
            pos: 0.0,
            step: 220.0 * 2.0 * std::f32::consts::PI / 44100.0,
        };
        player
            .build_sink_with_fade(source, PlaybackRuntimeReplacePolicy::FadeOutPrevious)
            .ok()?;
        player.started_at = Some(Instant::now());
        Some(player)
    }

    #[cfg(test)]
    pub(crate) fn aligned_span_seconds_for_tests(span_seconds: f32, sample_rate: u32) -> f32 {
        Self::aligned_span_duration(span_seconds, sample_rate).as_secs_f32()
    }
}

fn anti_clip_settings(enabled: bool, fade_ms: f32) -> (bool, Duration) {
    if !enabled {
        return (false, Duration::ZERO);
    }
    let default_fade_ms = DEFAULT_ANTI_CLIP_FADE.as_secs_f32() * 1000.0;
    let fade_ms = if !fade_ms.is_finite() || fade_ms < 0.0 {
        default_fade_ms
    } else {
        fade_ms.min(50.0)
    };
    // Convert through f64 so the documented 50 ms cap does not inherit the
    // positive rounding error in `50.0f32 / 1000.0` (50,000,001 ns).
    // Duration::from_secs_f64 rounds fractional nanoseconds to the nearest ns.
    (true, Duration::from_secs_f64(f64::from(fade_ms) / 1000.0))
}

fn wav_file_metadata(path: &std::path::Path) -> Result<(f32, u32, usize), String> {
    let reader = hound::WavReader::open(path)
        .map_err(|err| format!("Failed to open WAV file {}: {err}", path.display()))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate.max(1);
    let channels = usize::from(spec.channels).max(1);
    let frames = u64::from(reader.duration());
    if frames == 0 {
        return Err(format!(
            "WAV file contains no audio frames: {}",
            path.display()
        ));
    }
    Ok((frames as f32 / sample_rate as f32, sample_rate, channels))
}

fn test_audio_output_enabled() -> bool {
    std::env::var("YIELD_TEST_AUDIO_OUTPUT")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{anti_clip_settings, wav_file_metadata};
    use std::time::Duration;

    #[test]
    fn anti_click_settings_keep_legacy_default_and_bound_invalid_values() {
        assert_eq!(
            anti_clip_settings(true, f32::NAN),
            (true, Duration::from_millis(3))
        );
        assert_eq!(
            anti_clip_settings(true, f32::INFINITY),
            (true, Duration::from_millis(3))
        );
        assert_eq!(
            anti_clip_settings(true, -1.0),
            (true, Duration::from_millis(3))
        );
        assert_eq!(anti_clip_settings(true, 0.0), (true, Duration::ZERO));
        assert_eq!(
            anti_clip_settings(true, 2.0),
            (true, Duration::from_millis(2))
        );
        assert_eq!(
            anti_clip_settings(true, 3.0),
            (true, Duration::from_millis(3))
        );
        assert_eq!(
            anti_clip_settings(true, 90.0),
            (true, Duration::from_millis(50))
        );
        assert_eq!(anti_clip_settings(false, 3.0), (false, Duration::ZERO));
    }

    #[test]
    fn wav_file_metadata_probes_original_file_without_decoding_samples() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("runtime.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create WAV");
        for _ in 0..48_000 {
            writer.write_sample(0_i16).expect("write left");
            writer.write_sample(0_i16).expect("write right");
        }
        writer.finalize().expect("finalize WAV");

        let (duration, sample_rate, channels) =
            wav_file_metadata(&path).expect("probe WAV metadata");

        assert!((duration - 1.0).abs() < f32::EPSILON);
        assert_eq!(sample_rate, 48_000);
        assert_eq!(channels, 2);
    }
}
