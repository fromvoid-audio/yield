use std::{f32::consts::TAU, time::Duration};

use crate::Source;

use super::PlaybackSpanHandle;

const BEAT_AMPLITUDE: f32 = 0.22;
const OFFBEAT_AMPLITUDE: f32 = 0.13;
const BEAT_FREQUENCY_HZ: f32 = 2_400.0;
const OFFBEAT_FREQUENCY_HZ: f32 = 1_250.0;
const BEAT_DURATION_SECONDS: f32 = 0.028;
const OFFBEAT_DURATION_SECONDS: f32 = 0.022;

/// Click-track configuration attached to a playback request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackMetronomeConfig {
    beat_count: u16,
    cycle: Option<PlaybackMetronomeCycle>,
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
        }
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
    channels: u16,
    sample_rate: u32,
    live_control: Option<PlaybackSpanHandle>,
    live_revision: u64,
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
            channels,
            sample_rate,
            live_control: None,
            live_revision: 0,
        }
    }

    pub(super) fn new_live(inner: S, live_control: PlaybackSpanHandle) -> Self {
        let applied = live_control.applied_metronome();
        let channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate().max(1);
        Self {
            inner,
            config: PlaybackMetronomeConfig::new(applied.beat_count()),
            enabled: applied.enabled(),
            cycle_frames: applied.cycle_frames().max(1),
            cycle_offset_frames: applied.phase_frames() % applied.cycle_frames().max(1),
            sample_index: 0,
            channels,
            sample_rate,
            live_control: Some(live_control),
            live_revision: applied.revision(),
        }
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
        self.config = PlaybackMetronomeConfig::new(applied.beat_count());
        self.cycle_frames = applied.cycle_frames().max(1);
        self.cycle_offset_frames = applied.phase_frames() % self.cycle_frames;
        self.sample_index = 0;
    }

    fn click_value(&self, frame_index: u64) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let frame_in_cycle = (self.cycle_offset_frames + frame_index) % self.cycle_frames.max(1);
        let beat_interval = self.cycle_frames as f64 / f64::from(self.config.beat_count());
        if beat_interval <= f64::EPSILON {
            return 0.0;
        }

        let beat_phase = frame_in_cycle as f64 % beat_interval;
        if let Some(value) = click_tone(
            beat_phase,
            BEAT_DURATION_SECONDS,
            self.sample_rate,
            BEAT_FREQUENCY_HZ,
            BEAT_AMPLITUDE,
        ) {
            return value;
        }

        let offbeat_start = beat_interval * 0.5;
        if beat_phase < offbeat_start {
            return 0.0;
        }
        click_tone(
            beat_phase - offbeat_start,
            OFFBEAT_DURATION_SECONDS,
            self.sample_rate,
            OFFBEAT_FREQUENCY_HZ,
            OFFBEAT_AMPLITUDE,
        )
        .unwrap_or(0.0)
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
        }
        let frame_index = self.sample_index / u64::from(self.channels);
        let click = self.click_value(frame_index);
        self.sample_index = self.sample_index.saturating_add(1);
        if self.enabled {
            Some((dry + click).clamp(-1.0, 1.0))
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
    duration_seconds: f32,
    sample_rate: u32,
    frequency_hz: f32,
    amplitude: f32,
) -> Option<f32> {
    let duration_frames = f64::from(duration_seconds) * f64::from(sample_rate);
    if frames_since_event >= duration_frames {
        return None;
    }
    let position = (frames_since_event / duration_frames.max(1.0)).clamp(0.0, 1.0) as f32;
    let envelope = (1.0 - position).powi(3);
    let phase = (frames_since_event as f32 / sample_rate as f32) * frequency_hz * TAU
        + std::f32::consts::FRAC_PI_2;
    Some(phase.sin() * amplitude * envelope)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{MetronomeSource, PlaybackMetronomeConfig};
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
