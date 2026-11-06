use std::{
    f32::consts::TAU,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use crate::Source;

use super::PlaybackSpanHandle;
use super::playback_span_handle::add_modulo;

const BEAT_AMPLITUDE: f32 = 0.22;
const OFFBEAT_AMPLITUDE: f32 = 0.13;
const BEAT_FREQUENCY_HZ: f32 = 2_400.0;
const OFFBEAT_FREQUENCY_HZ: f32 = 1_250.0;
const BEAT_DURATION_SECONDS: f32 = 0.028;
const OFFBEAT_DURATION_SECONDS: f32 = 0.022;

/// Built-in metronome sounds synthesized without loading an asset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum MetronomeSound {
    /// The original short click.
    #[default]
    Classic = 0,
    /// A hollow struck tone.
    Woodblock = 1,
    /// A short pitched beep.
    ElectronicBeep = 2,
    /// A brief percussive noise and tone.
    Rimshot = 3,
}

/// Click-only level, rhythmic layers and sound, independent of the beat grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackMetronomeMix {
    volume_percent: u8,
    beats: bool,
    offbeats: bool,
    sound: MetronomeSound,
}

impl Default for PlaybackMetronomeMix {
    fn default() -> Self {
        Self::new(100, true, true, MetronomeSound::Classic)
    }
}

impl PlaybackMetronomeMix {
    /// Build a mix, clamping its click level to 100%.
    pub fn new(volume_percent: u8, beats: bool, offbeats: bool, sound: MetronomeSound) -> Self {
        Self {
            volume_percent: volume_percent.min(100),
            beats,
            offbeats,
            sound,
        }
    }

    pub(super) fn encode(self) -> u16 {
        u16::from(self.volume_percent)
            | (u16::from(self.beats) << 8)
            | (u16::from(self.offbeats) << 9)
            | (u16::from(self.sound as u8) << 10)
    }

    pub(super) fn decode(bits: u16) -> Self {
        let sound = match bits >> 10 {
            1 => MetronomeSound::Woodblock,
            2 => MetronomeSound::ElectronicBeep,
            3 => MetronomeSound::Rimshot,
            _ => MetronomeSound::Classic,
        };
        Self::new(
            bits as u8,
            bits & (1 << 8) != 0,
            bits & (1 << 9) != 0,
            sound,
        )
    }
}

/// One optional mix for current and future sources owned by a playback runtime.
#[derive(Clone, Debug)]
pub(super) struct MetronomeMixOverride(Arc<AtomicU16>);

impl Default for MetronomeMixOverride {
    fn default() -> Self {
        Self(Arc::new(AtomicU16::new(Self::UNSET)))
    }
}

impl MetronomeMixOverride {
    const UNSET: u16 = u16::MAX;

    pub(super) fn set(&self, mix: PlaybackMetronomeMix) {
        // The entire update fits in one word and is independent of grid or source lifetime.
        self.0.store(mix.encode(), Ordering::Relaxed);
    }

    fn get(&self) -> Option<PlaybackMetronomeMix> {
        let bits = self.0.load(Ordering::Relaxed);
        (bits != Self::UNSET).then(|| PlaybackMetronomeMix::decode(bits))
    }
}

/// Click-track configuration attached to a playback request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackMetronomeConfig {
    beat_count: u16,
    cycle: Option<PlaybackMetronomeCycle>,
    mix: PlaybackMetronomeMix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaybackMetronomeCycle {
    kind: PlaybackMetronomeCycleKind,
    frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackMetronomeCycleKind {
    Relative { offset_frames: u64 },
    Source { origin_frame: u64 },
}

impl PlaybackMetronomeConfig {
    /// Create a metronome grid with one beat at each equal division boundary.
    ///
    /// `beat_count` is clamped to at least one so callers can pass UI counter
    /// values directly without creating a silent, invalid request.
    pub fn new(beat_count: u16) -> Self {
        Self {
            beat_count: beat_count.max(1),
            cycle: None,
            mix: PlaybackMetronomeMix::default(),
        }
    }

    /// Apply click-only settings without changing the beat grid.
    pub fn with_mix(mut self, mix: PlaybackMetronomeMix) -> Self {
        self.mix = mix;
        self
    }

    /// Return the click-only settings captured with the request.
    pub fn mix(self) -> PlaybackMetronomeMix {
        self.mix
    }

    /// Use an explicit click-cycle length and starting offset.
    ///
    /// This lets host applications restart playback from the middle of a
    /// visible grid while keeping the audible metronome phase aligned to that
    /// original grid.
    pub fn with_cycle(mut self, frames: u64, offset_frames: u64) -> Self {
        let frames = frames.max(1);
        self.cycle = Some(PlaybackMetronomeCycle {
            kind: PlaybackMetronomeCycleKind::Relative {
                offset_frames: offset_frames % frames,
            },
            frames,
        });
        self
    }

    /// Use an explicit click-cycle length anchored to an absolute source frame.
    ///
    /// `origin_frame` is the decoded source frame at which the cycle starts.
    /// The phase is resolved when a playback plan is created, so retargeting a
    /// live source keeps the click grid tied to source position rather than a
    /// request-time UI offset.
    pub fn with_source_cycle(mut self, frames: u64, origin_frame: u64) -> Self {
        self.cycle = Some(PlaybackMetronomeCycle {
            kind: PlaybackMetronomeCycleKind::Source { origin_frame },
            frames: frames.max(1),
        });
        self
    }

    /// Return the effective beat count after configuration normalization.
    pub fn beat_count(self) -> u16 {
        self.beat_count
    }

    pub(crate) fn cycle_at_anchor(
        self,
        default_frames: u64,
        default_offset_frames: u64,
        anchor_frame: u64,
    ) -> (u64, u64) {
        let default_frames = default_frames.max(1);
        match self.cycle {
            Some(cycle) => match cycle.kind {
                PlaybackMetronomeCycleKind::Relative { offset_frames } => {
                    (cycle.frames.max(1), offset_frames % cycle.frames.max(1))
                }
                PlaybackMetronomeCycleKind::Source { origin_frame } => (
                    cycle.frames.max(1),
                    source_phase(anchor_frame, origin_frame, cycle.frames),
                ),
            },
            None => (default_frames, default_offset_frames % default_frames),
        }
    }
}

fn source_phase(anchor_frame: u64, origin_frame: u64, cycle_frames: u64) -> u64 {
    let cycle_frames = cycle_frames.max(1);
    if anchor_frame >= origin_frame {
        return anchor_frame.saturating_sub(origin_frame) % cycle_frames;
    }

    let distance = origin_frame.saturating_sub(anchor_frame) % cycle_frames;
    if distance == 0 {
        0
    } else {
        cycle_frames - distance
    }
}

pub(super) struct MetronomeSource<S> {
    inner: S,
    config: PlaybackMetronomeConfig,
    enabled: bool,
    cycle_frames: u64,
    cycle_offset_frames: u64,
    sample_index: u64,
    frame_click: f32,
    channels: u16,
    sample_rate: u32,
    live_control: Option<PlaybackSpanHandle>,
    live_revision: u64,
    mix_override: Option<MetronomeMixOverride>,
}

impl<S> MetronomeSource<S>
where
    S: Source,
{
    pub(super) fn new(
        inner: S,
        config: PlaybackMetronomeConfig,
        cycle_frames: u64,
        cycle_offset_frames: u64,
        anchor_frame: u64,
    ) -> Self {
        let (cycle_frames, cycle_offset_frames) =
            config.cycle_at_anchor(cycle_frames, cycle_offset_frames, anchor_frame);
        let channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate().max(1);
        Self {
            inner,
            config,
            enabled: true,
            cycle_frames,
            cycle_offset_frames,
            sample_index: 0,
            frame_click: 0.0,
            channels,
            sample_rate,
            live_control: None,
            live_revision: 0,
            mix_override: None,
        }
    }

    pub(super) fn new_live(inner: S, live_control: PlaybackSpanHandle) -> Self {
        let applied = live_control.applied_metronome();
        let channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate().max(1);
        Self {
            inner,
            config: PlaybackMetronomeConfig::new(applied.beat_count()).with_mix(applied.mix()),
            enabled: applied.enabled(),
            cycle_frames: applied.cycle_frames().max(1),
            cycle_offset_frames: applied.phase_frames() % applied.cycle_frames().max(1),
            sample_index: 0,
            frame_click: 0.0,
            channels,
            sample_rate,
            live_control: Some(live_control),
            live_revision: applied.revision(),
            mix_override: None,
        }
    }

    pub(super) fn with_mix_override(mut self, mix_override: MetronomeMixOverride) -> Self {
        self.mix_override = Some(mix_override);
        self
    }

    fn refresh_live_control(&mut self) {
        let Some(control) = self.live_control.as_ref() else {
            return;
        };
        let Some(applied) = control.try_applied_metronome() else {
            return;
        };
        if applied.revision() == self.live_revision {
            return;
        }
        self.live_revision = applied.revision();
        self.enabled = applied.enabled();
        self.config = PlaybackMetronomeConfig::new(applied.beat_count()).with_mix(applied.mix());
        self.cycle_frames = applied.cycle_frames().max(1);
        self.cycle_offset_frames = applied.phase_frames() % self.cycle_frames;
        self.sample_index = 0;
    }

    fn click_value(&self, frame_index: u64) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let mix = self
            .mix_override
            .as_ref()
            .and_then(MetronomeMixOverride::get)
            .unwrap_or_else(|| self.config.mix());
        if mix.volume_percent == 0 || !(mix.beats || mix.offbeats) {
            return 0.0;
        }
        let cycle = self.cycle_frames;
        let frame_in_cycle = self
            .cycle_offset_frames
            .checked_add(frame_index)
            .map_or_else(
                || add_modulo(self.cycle_offset_frames, frame_index, cycle),
                |frame| frame % cycle,
            );
        let beats = u64::from(self.config.beat_count());
        // The numerator keeps fractional beat and half-beat boundaries exact.
        let phase = frame_in_cycle.checked_mul(beats).map_or_else(
            || ((u128::from(frame_in_cycle) * u128::from(beats)) % u128::from(cycle)) as u64,
            |numerator| numerator % cycle,
        );
        let (elapsed, offbeat) = if mix.offbeats && phase >= cycle.div_ceil(2) {
            let elapsed = (phase - cycle / 2) as f64 - if cycle % 2 == 0 { 0.0 } else { 0.5 };
            (elapsed / beats as f64, true)
        } else if mix.beats {
            (phase as f64 / beats as f64, false)
        } else {
            return 0.0;
        };
        click_tone(elapsed, self.sample_rate, offbeat, mix.sound)
            * (f32::from(mix.volume_percent) / 100.0)
    }
}

impl<S> Iterator for MetronomeSource<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let dry = self.inner.next()?;
        if self.sample_index.is_multiple_of(u64::from(self.channels)) {
            self.refresh_live_control();
            self.frame_click = self.click_value(self.sample_index / u64::from(self.channels));
        }
        self.sample_index = self.sample_index.saturating_add(1);
        if self.frame_click != 0.0 {
            Some((dry + self.frame_click).clamp(-1.0, 1.0))
        } else {
            Some(dry)
        }
    }
}

impl<S> Source for MetronomeSource<S>
where
    S: Source,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

fn click_tone(
    frames_since_event: f64,
    sample_rate: u32,
    offbeat: bool,
    sound: MetronomeSound,
) -> f32 {
    let (duration_seconds, amplitude) = if offbeat {
        (OFFBEAT_DURATION_SECONDS, OFFBEAT_AMPLITUDE)
    } else {
        (BEAT_DURATION_SECONDS, BEAT_AMPLITUDE)
    };
    let duration_frames = f64::from(duration_seconds) * f64::from(sample_rate);
    if frames_since_event >= duration_frames {
        return 0.0;
    }
    let position = (frames_since_event / duration_frames.max(1.0)).clamp(0.0, 1.0) as f32;
    let envelope = match sound {
        MetronomeSound::ElectronicBeep => (1.0 - position).powi(2),
        MetronomeSound::Rimshot => (1.0 - position).powi(5),
        _ => (1.0 - position).powi(3),
    };
    let elapsed = frames_since_event as f32 / sample_rate as f32;
    let tone = |frequency: f32| (elapsed * frequency * TAU + std::f32::consts::FRAC_PI_2).sin();
    let pitch = if offbeat { 0.72 } else { 1.0 };
    let wave = match sound {
        MetronomeSound::Classic => tone(if offbeat {
            OFFBEAT_FREQUENCY_HZ
        } else {
            BEAT_FREQUENCY_HZ
        }),
        MetronomeSound::Woodblock => {
            0.78 * tone(820.0 * pitch) + 0.22 * tone(1_670.0 * pitch) * (1.0 - position)
        }
        MetronomeSound::ElectronicBeep => tone(1_050.0 * pitch),
        MetronomeSound::Rimshot => {
            let noise_bits = (frames_since_event as u32)
                .wrapping_add(1)
                .wrapping_mul(0x9e37_79b9)
                .rotate_left(13);
            let noise = (noise_bits >> 16) as f32 / 32_767.5 - 1.0;
            0.65 * noise + 0.35 * tone(1_750.0 * pitch)
        }
    };
    wave * amplitude * envelope
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{MetronomeSound, MetronomeSource, PlaybackMetronomeConfig, PlaybackMetronomeMix};
    use crate::{SamplesBuffer, Source};

    #[test]
    fn metronome_source_accents_beats_and_uses_distinct_offbeats() {
        let samples = Arc::<[f32]>::from(vec![0.0; 48_000]);
        let source = SamplesBuffer::from_arc(1, 48_000, samples);
        let mut metronome =
            MetronomeSource::new(source, PlaybackMetronomeConfig::new(4), 48_000, 0, 0);

        assert!(metronome.next().expect("downbeat") > 0.20);
        for _ in 1..6_000 {
            metronome.next();
        }
        let offbeat = metronome.next().expect("offbeat");
        assert!((0.11..0.15).contains(&offbeat));

        for _ in 6_001..12_000 {
            metronome.next();
        }
        assert!(metronome.next().expect("beat") > 0.20);
    }

    #[test]
    fn layer_choices_use_the_same_fractional_grid_and_preserve_dry_audio() {
        for (beats, offbeats) in [(true, true), (true, false), (false, true), (false, false)] {
            let mix = PlaybackMetronomeMix::new(100, beats, offbeats, MetronomeSound::Classic);
            let config = PlaybackMetronomeConfig::new(3)
                .with_cycle(1_001, 0)
                .with_mix(mix);
            let dry = 0.25;
            let source = SamplesBuffer::from_arc(1, 1_000, Arc::from(vec![dry; 1_001]));
            let frames: Vec<_> = MetronomeSource::new(source, config, 1_001, 0, 0).collect();
            assert_eq!(frames[0] != dry, beats);
            for first_frame in [167, 501, 835] {
                assert_eq!(frames[first_frame - 1], dry);
                assert_eq!(frames[first_frame] != dry, offbeats);
            }
            assert_eq!(frames[334] != dry, beats);
            assert_eq!(frames[668] != dry, beats);
            assert_eq!(frames[300], dry);
        }
    }

    #[test]
    fn built_in_clicks_are_distinct_and_only_click_level_changes() {
        let render = |sound, volume, dry| {
            let mix = PlaybackMetronomeMix::new(volume, true, false, sound);
            let config = PlaybackMetronomeConfig::new(1).with_mix(mix);
            let source = SamplesBuffer::from_arc(1, 48_000, Arc::from(vec![dry; 256]));
            MetronomeSource::new(source, config, 48_000, 0, 0).collect::<Vec<_>>()
        };
        let mut distinct = Vec::new();
        for sound in [
            MetronomeSound::Classic,
            MetronomeSound::Woodblock,
            MetronomeSound::ElectronicBeep,
            MetronomeSound::Rimshot,
        ] {
            let full = render(sound, 100, 0.0);
            assert!(full.iter().any(|sample| sample.abs() > 0.09));
            assert!(full.iter().all(|sample| sample.abs() <= 0.221));
            assert!(!distinct.contains(&full));
            assert_eq!(render(sound, 255, 0.0), full);
            assert_eq!(render(sound, 0, 1.5), vec![1.5; 256]);
            for (actual, click) in render(sound, 25, 0.25).into_iter().zip(&full) {
                assert!((actual - (0.25 + 0.25 * click)).abs() < 0.000_001);
            }
            distinct.push(full);
        }
    }

    #[test]
    fn metronome_source_honors_loop_offset_phase() {
        let samples = Arc::<[f32]>::from(vec![0.0; 48_000]);
        let source = SamplesBuffer::from_arc(1, 48_000, samples);
        let mut metronome =
            MetronomeSource::new(source, PlaybackMetronomeConfig::new(4), 48_000, 6_000, 0);

        let first = metronome.next().expect("offset starts on offbeat");
        assert!((0.11..0.15).contains(&first));
    }

    #[test]
    fn metronome_source_can_use_explicit_grid_cycle() {
        let samples = Arc::<[f32]>::from(vec![0.0; 24_000]);
        let source = SamplesBuffer::from_arc(1, 48_000, samples);
        let config = PlaybackMetronomeConfig::new(4).with_cycle(48_000, 6_000);
        let mut metronome = MetronomeSource::new(source, config, 24_000, 0, 0);

        let first = metronome.next().expect("explicit cycle offbeat");

        assert!((0.11..0.15).contains(&first));
    }

    #[test]
    fn source_cycle_resolves_against_the_absolute_anchor() {
        let config = PlaybackMetronomeConfig::new(4).with_source_cycle(1_000, 100);

        assert_eq!(config.cycle_at_anchor(1, 0, 1_125), (1_000, 25));
    }

    #[test]
    fn source_cycle_phase_handles_before_origin_and_u64_extremes() {
        let before_origin = PlaybackMetronomeConfig::new(1).with_source_cycle(10, 10);
        assert_eq!(before_origin.cycle_at_anchor(1, 0, 3), (10, 3));

        let max_origin = PlaybackMetronomeConfig::new(1).with_source_cycle(10, u64::MAX);
        assert_eq!(max_origin.cycle_at_anchor(1, 0, 0), (10, 5));

        let max_cycle = PlaybackMetronomeConfig::new(1).with_source_cycle(u64::MAX, 0);
        assert_eq!(max_cycle.cycle_at_anchor(1, 0, u64::MAX), (u64::MAX, 0));

        let wrapped_before_origin =
            PlaybackMetronomeConfig::new(1).with_source_cycle(u64::MAX - 1, u64::MAX);
        assert_eq!(
            wrapped_before_origin.cycle_at_anchor(1, 0, 0),
            (u64::MAX - 1, u64::MAX - 2)
        );

        let unit_cycle = PlaybackMetronomeConfig::new(1).with_source_cycle(0, u64::MAX);
        assert_eq!(unit_cycle.cycle_at_anchor(1, 0, u64::MAX), (1, 0));
    }

    #[test]
    fn relative_cycle_semantics_ignore_source_anchor_resolution() {
        let config = PlaybackMetronomeConfig::new(1).with_cycle(10, 3);

        assert_eq!(config.cycle_at_anchor(1, 0, 999), (10, 3));
    }

    #[test]
    fn metronome_source_preserves_source_layout_and_duration() {
        let samples = Arc::<[f32]>::from(vec![0.1; 96_000]);
        let source = SamplesBuffer::from_arc(2, 48_000, samples);
        let mut metronome =
            MetronomeSource::new(source, PlaybackMetronomeConfig::new(4), 48_000, 0, 0);

        assert_eq!(metronome.channels(), 2);
        assert_eq!(metronome.sample_rate(), 48_000);
        assert_eq!(
            metronome.total_duration(),
            Some(std::time::Duration::from_secs(1))
        );
        assert!(metronome.next().expect("left beat") > 0.30);
        assert!(metronome.next().expect("right beat") > 0.30);
    }
}
