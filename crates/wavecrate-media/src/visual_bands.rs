//! Bounded frequency-layer projection for waveform previews.

const BAND_COUNT: usize = 4;
const VISUAL_BAND_COUNT: usize = 3;

/// Process one mono sample at a time into low, mid, high, and raw lanes.
struct VisualBandFrameProcessor {
    alpha_low: f32,
    alpha_mid: f32,
    low: f32,
    mid_low: f32,
    low_envelope: f32,
    mid_envelope: f32,
    high_envelope: f32,
    low_release: f32,
    mid_release: f32,
    high_release: f32,
}

impl VisualBandFrameProcessor {
    fn new(sample_rate: u32) -> Self {
        Self {
            alpha_low: lowpass_alpha(sample_rate, 150.0),
            alpha_mid: lowpass_alpha(sample_rate, 2_200.0),
            low: 0.0,
            mid_low: 0.0,
            low_envelope: 0.0,
            mid_envelope: 0.0,
            high_envelope: 0.0,
            low_release: envelope_release_alpha(sample_rate, 12.0),
            mid_release: envelope_release_alpha(sample_rate, 5.5),
            high_release: envelope_release_alpha(sample_rate, 2.2),
        }
    }

    fn process(&mut self, sample: f32) -> [f32; BAND_COUNT] {
        let sample = sanitize_sample(sample);
        self.low += self.alpha_low * (sample - self.low);
        self.mid_low += self.alpha_mid * (sample - self.mid_low);
        let low_band = (self.low * 1.08).clamp(-1.0, 1.0);
        let mid_band = ((self.mid_low - self.low) * 1.45).clamp(-1.0, 1.0);
        let high_band = ((sample - self.mid_low) * 2.15).clamp(-1.0, 1.0);
        self.low_envelope =
            follow_visual_envelope(self.low_envelope, low_band.abs(), self.low_release);
        self.mid_envelope =
            follow_visual_envelope(self.mid_envelope, mid_band.abs(), self.mid_release);
        self.high_envelope =
            follow_visual_envelope(self.high_envelope, high_band.abs(), self.high_release);
        [
            self.low_envelope,
            self.mid_envelope,
            self.high_envelope,
            sample,
        ]
    }
}

/// Incrementally project a known-length mono stream into bounded visual columns.
pub(super) struct StreamingProjector {
    processor: VisualBandFrameProcessor,
    total_frames: usize,
    point_count: usize,
    columns: Vec<[f32; BAND_COUNT]>,
}

impl StreamingProjector {
    /// Create a projector sized to the source frame count and column limit.
    pub(super) fn new(sample_rate: u32, total_frames: usize, max_points: usize) -> Self {
        let point_count = total_frames.min(max_points);
        Self {
            processor: VisualBandFrameProcessor::new(sample_rate),
            total_frames,
            point_count,
            columns: vec![[0.0_f32; BAND_COUNT]; point_count],
        }
    }

    /// Add one mono source frame at its full-file position.
    pub(super) fn push(&mut self, frame_index: usize, sample: f32) {
        if self.point_count == 0 || frame_index >= self.total_frames {
            return;
        }
        let frame = self.processor.process(sample);
        let column =
            ((frame_index as u128 * self.point_count as u128) / self.total_frames as u128) as usize;
        let Some(column_values) = self.columns.get_mut(column) else {
            return;
        };
        for lane in 0..VISUAL_BAND_COUNT {
            column_values[lane] = column_values[lane].max(frame[lane]);
        }
        column_values[3] = column_values[3].max(frame[3].abs());
    }

    /// Warm the filter and envelope state without adding a visible column.
    pub(super) fn warmup(&mut self, sample: f32) {
        self.processor.process(sample);
    }

    /// Finish the projection and return the gains used for its file-level normalization.
    pub(super) fn finish(mut self) -> (Vec<[f32; BAND_COUNT]>, [f32; VISUAL_BAND_COUNT]) {
        let raw_peak = self
            .columns
            .iter()
            .map(|column| column[3])
            .fold(0.0_f32, f32::max);
        let peaks = [
            self.columns
                .iter()
                .map(|column| column[0])
                .fold(0.0_f32, f32::max),
            self.columns
                .iter()
                .map(|column| column[1])
                .fold(0.0_f32, f32::max),
            self.columns
                .iter()
                .map(|column| column[2])
                .fold(0.0_f32, f32::max),
        ];
        let gains = visual_band_normalization(raw_peak, peaks);
        self.apply_gains(gains);
        (self.columns, gains)
    }

    /// Finish the projection with previously computed file-level normalization gains.
    pub(super) fn finish_with_gains(
        mut self,
        gains: [f32; VISUAL_BAND_COUNT],
    ) -> Vec<[f32; BAND_COUNT]> {
        let gains = if gains
            .iter()
            .all(|gain| gain.is_finite() && *gain > 0.0 && *gain <= 4.0)
        {
            gains
        } else {
            [1.0; VISUAL_BAND_COUNT]
        };
        self.apply_gains(gains);
        self.columns
    }

    fn apply_gains(&mut self, gains: [f32; VISUAL_BAND_COUNT]) {
        for column in &mut self.columns {
            for (band, gain) in gains.into_iter().enumerate() {
                column[band] = finite_unit(column[band] * gain);
            }
            column[3] = finite_unit(column[3]);
        }
    }
}

/// Project mono samples into bounded per-column frequency-layer peak magnitudes.
#[cfg(test)]
pub(super) fn project(samples: &[f32], sample_rate: u32, max_points: usize) -> Vec<[f32; 4]> {
    if samples.is_empty() || max_points == 0 {
        return Vec::new();
    }

    let mut projector = StreamingProjector::new(sample_rate, samples.len(), max_points);
    for (index, sample) in samples.iter().copied().enumerate() {
        projector.push(index, sample);
    }
    projector.finish().0
}

fn visual_band_normalization(
    raw_peak: f32,
    peaks: [f32; VISUAL_BAND_COUNT],
) -> [f32; VISUAL_BAND_COUNT] {
    if raw_peak <= 0.000_01 || !raw_peak.is_finite() {
        return [1.0; VISUAL_BAND_COUNT];
    }
    let spectral_total = peaks.iter().copied().sum::<f32>().max(0.000_01);
    let targets = [raw_peak * 0.98, raw_peak * 0.74, raw_peak * 0.34];
    let boost_thresholds = [raw_peak * 0.08, raw_peak * 0.05, raw_peak * 0.035];
    let max_gains = [2.6_f32, 2.8, 2.4];
    let mut gains = [1.0; VISUAL_BAND_COUNT];
    for band in 0..VISUAL_BAND_COUNT {
        let peak = peaks[band];
        if peak <= 0.000_01 || !peak.is_finite() {
            continue;
        }
        let energy_share = peak / spectral_total;
        let target = targets[band] * smoothstep_scalar(0.12, 0.55, energy_share);
        let max_gain = if peak < boost_thresholds[band] {
            1.0
        } else {
            max_gains[band]
        };
        gains[band] = (target / peak).clamp(0.25, max_gain);
    }
    if gains
        .iter()
        .all(|gain| gain.is_finite() && *gain > 0.0 && *gain <= 4.0)
    {
        gains
    } else {
        [1.0; VISUAL_BAND_COUNT]
    }
}

fn sanitize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn smoothstep_scalar(edge0: f32, edge1: f32, value: f32) -> f32 {
    let range = (edge1 - edge0).abs().max(0.000_01);
    let t = ((value - edge0) / range).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn follow_visual_envelope(previous: f32, value: f32, release_alpha: f32) -> f32 {
    if value >= previous {
        value
    } else {
        previous + release_alpha * (value - previous)
    }
}

fn envelope_release_alpha(sample_rate: u32, release_ms: f32) -> f32 {
    let samples = sample_rate.max(1) as f32 * (release_ms.max(0.1) / 1_000.0);
    (1.0 - (-1.0 / samples).exp()).clamp(0.0, 1.0)
}

fn lowpass_alpha(sample_rate: u32, cutoff_hz: f32) -> f32 {
    (1.0 - (-std::f32::consts::TAU * cutoff_hz / sample_rate.max(1) as f32).exp()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn silence_stays_zero_and_finite() {
        let columns = project(&[0.0; 8_000], 16_000, 2_048);
        assert!(!columns.is_empty());
        assert!(
            columns
                .iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value) && *value == 0.0)
        );
    }

    #[test]
    fn nonfinite_samples_are_sanitized() {
        let columns = project(
            &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25],
            16_000,
            64,
        );
        assert!(
            columns
                .iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        );
        assert!(columns.iter().take(3).flatten().all(|value| *value == 0.0));
    }

    #[test]
    fn output_is_bounded_to_the_requested_columns() {
        let samples = (0..100_000)
            .map(|index| (index as f32 * TAU * 440.0 / 16_000.0).sin())
            .collect::<Vec<_>>();
        let columns = project(&samples, 16_000, 2_048);
        assert_eq!(columns.len(), 2_048);
        assert!(
            columns
                .iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        );
    }

    #[test]
    fn low_and_high_sines_use_distinct_layers() {
        let sample_rate = 16_000;
        let low = (0..sample_rate)
            .map(|index| (index as f32 * TAU * 70.0 / sample_rate as f32).sin())
            .collect::<Vec<_>>();
        let high = (0..sample_rate)
            .map(|index| (index as f32 * TAU * 6_000.0 / sample_rate as f32).sin())
            .collect::<Vec<_>>();
        let low_bands = project(&low, sample_rate, 256);
        let high_bands = project(&high, sample_rate, 256);
        let low_peak = low_bands.iter().map(|column| column[0]).fold(0.0, f32::max);
        let low_high_peak = low_bands.iter().map(|column| column[2]).fold(0.0, f32::max);
        let high_low_peak = high_bands
            .iter()
            .map(|column| column[0])
            .fold(0.0, f32::max);
        let high_high_peak = high_bands
            .iter()
            .map(|column| column[2])
            .fold(0.0, f32::max);
        assert!(low_peak > low_high_peak * 2.0);
        assert!(high_high_peak > high_low_peak * 2.0);
    }

    #[test]
    fn normalization_preserves_short_low_content_and_limits_high_content() {
        let sample_rate = 48_000;
        let low = (0..2_656)
            .map(|frame| (frame as f32 * TAU * 72.0 / sample_rate as f32).sin())
            .collect::<Vec<_>>();
        let low_bands = project(&low, sample_rate, 256);
        let low_peak = low_bands.iter().map(|column| column[0]).fold(0.0, f32::max);
        let raw_peak = low_bands.iter().map(|column| column[3]).fold(0.0, f32::max);
        assert!(raw_peak > 0.99);
        assert!(low_peak > raw_peak * 0.94);

        let high = (0..sample_rate / 100)
            .map(|frame| (frame as f32 * TAU * 7_200.0 / sample_rate as f32).sin())
            .collect::<Vec<_>>();
        let high_bands = project(&high, sample_rate, 256);
        let high_low_peak = high_bands
            .iter()
            .map(|column| column[0])
            .fold(0.0, f32::max);
        let high_peak = high_bands
            .iter()
            .map(|column| column[2])
            .fold(0.0, f32::max);
        assert!(high_peak > 0.30);
        assert!(high_low_peak < high_peak * 0.35);
    }
}
