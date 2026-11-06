//! Prepared band-limited interpolation for one stereo program frame.

use std::f64::consts::PI;

const BASE_RADIUS: u64 = 80;
const MAX_RADIUS: u64 = 512;
const MAX_LOOKAHEAD_MS: u64 = 2;
const MAX_EXACT_PHASES: u32 = 320;
const INTERPOLATED_PHASES: u32 = 128;
const CUTOFF_FRACTION: f64 = 0.925;
const KAISER_BETA: f64 = 9.0;

pub(crate) struct ResampleKernel {
    radius: usize,
    target_rate: u32,
    phase_intervals: u32,
    weights: Box<[f32]>,
}

impl ResampleKernel {
    /// Prepare the coefficients off the audio callback. Large reduced phase
    /// denominators use a fixed-size table with an endpoint for interpolation.
    pub(crate) fn new(source_rate: u32, target_rate: u32) -> Self {
        let source_rate = source_rate.max(1);
        let target_rate = target_rate.max(1);
        let ratio_numerator = source_rate.max(target_rate);
        // Keep initial source demand within the short transient prefill budget.
        let time_limited_radius = (u64::from(source_rate) * MAX_LOOKAHEAD_MS / 1_000).max(1);
        let radius = (BASE_RADIUS * u64::from(ratio_numerator))
            .div_ceil(u64::from(target_rate))
            .min(time_limited_radius)
            .min(MAX_RADIUS) as usize;
        let reduced_denominator = target_rate / gcd(source_rate, target_rate);
        let phase_intervals = if reduced_denominator <= MAX_EXACT_PHASES {
            reduced_denominator
        } else {
            INTERPOLATED_PHASES
        };
        let row_count = phase_intervals + 1;
        let cutoff = CUTOFF_FRACTION * f64::from(target_rate) / f64::from(ratio_numerator);
        let window_scale = bessel_i0(KAISER_BETA) - 1.0;
        let mut weights = Vec::with_capacity(row_count as usize * 2 * radius);
        let mut row = vec![0.0; 2 * radius];
        for phase in 0..row_count {
            let fraction = f64::from(phase) / f64::from(phase_intervals);
            for (index, coefficient) in row.iter_mut().enumerate() {
                let distance = (index as isize + 1 - radius as isize) as f64 - fraction;
                let unit_distance = distance / radius as f64;
                let window_argument = KAISER_BETA * (1.0 - unit_distance.powi(2)).max(0.0).sqrt();
                // Zero at either endpoint makes the phase-one row a shifted phase-zero row.
                let window = (bessel_i0(window_argument) - 1.0) / window_scale;
                let angle = PI * cutoff * distance;
                let sinc = if angle == 0.0 {
                    1.0
                } else {
                    angle.sin() / angle
                };
                *coefficient = sinc * window;
            }
            let sum = row.iter().sum::<f64>();
            weights.extend(row.iter().map(|coefficient| (coefficient / sum) as f32));
        }
        Self {
            radius,
            target_rate,
            phase_intervals,
            weights: weights.into_boxed_slice(),
        }
    }

    pub(crate) fn radius(&self) -> usize {
        self.radius
    }

    /// Render from the `2 * radius` input frames spanning `1 - radius` through
    /// `radius` relative to the integer source position. `phase` is the exact
    /// position remainder over the target rate from the same rate pair.
    pub(crate) fn render(&self, frames: &[[f32; 2]], phase: u32) -> [f32; 2] {
        debug_assert_eq!(frames.len(), 2 * self.radius);
        self.render_frames(frames.iter().copied(), phase)
    }

    /// Hold the final in-range frame across any prefetched frames past the edge.
    pub(crate) fn render_at_right_edge(
        &self,
        frames: &[[f32; 2]],
        valid: usize,
        phase: u32,
    ) -> [f32; 2] {
        debug_assert_eq!(frames.len(), 2 * self.radius);
        let edge = frames[valid - 1];
        let input = frames[..valid]
            .iter()
            .copied()
            .chain(std::iter::repeat_n(edge, frames.len() - valid));
        self.render_frames(input, phase)
    }

    fn render_frames(&self, frames: impl Iterator<Item = [f32; 2]>, phase: u32) -> [f32; 2] {
        let taps = 2 * self.radius;
        debug_assert!(phase < self.target_rate);
        let table_phase = u64::from(phase) * u64::from(self.phase_intervals);
        let target_rate = u64::from(self.target_rate);
        let row_start = (table_phase / target_rate) as usize * taps;
        let lower = &self.weights[row_start..row_start + taps];
        let remainder = table_phase % target_rate;
        if remainder == 0 {
            return convolve(frames, lower.iter().copied());
        }
        let blend = remainder as f32 / self.target_rate as f32;
        let upper = &self.weights[row_start + taps..row_start + 2 * taps];
        convolve(
            frames,
            lower
                .iter()
                .zip(upper)
                .map(|(lo, hi)| lo + (hi - lo) * blend),
        )
    }
}

fn convolve(
    frames: impl Iterator<Item = [f32; 2]>,
    weights: impl Iterator<Item = f32>,
) -> [f32; 2] {
    let mut stereo = [0.0; 2];
    for ([left, right], weight) in frames.zip(weights) {
        stereo[0] += left * weight;
        stereo[1] += right * weight;
    }
    stereo
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn bessel_i0(x: f64) -> f64 {
    let quarter_square = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = term;
    // This coefficient builder only evaluates the series on [0, KAISER_BETA].
    for k in 1..=24 {
        term *= quarter_square / f64::from(k * k);
        sum += term;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_interpolated_tables_stay_bounded() {
        let exact = ResampleKernel::new(44_100, 96_000);
        assert_eq!(exact.radius(), 80);
        assert_eq!(exact.phase_intervals, 320);
        assert_eq!(exact.weights.len(), 321 * 160);

        let interpolated = ResampleKernel::new(48_001, 44_100);
        assert_eq!(interpolated.phase_intervals, INTERPOLATED_PHASES);
        assert_eq!(interpolated.weights.len(), 129 * 176);

        let extreme = ResampleKernel::new(u32::MAX - 2, 320);
        assert_eq!(extreme.radius(), MAX_RADIUS as usize);
        assert_eq!(extreme.weights.len(), 321 * 2 * MAX_RADIUS as usize);
    }

    #[test]
    fn lookahead_respects_source_time_and_retains_the_common_rate_filters() {
        for (source, target, expected) in [
            (499, 48_000, 1),
            (8_000, 48_000, 16),
            (44_100, 48_000, 80),
            (48_000, 16_000, 96),
            (48_000, 44_100, 88),
            (96_000, 44_100, 175),
            (384_000, 8_000, 512),
        ] {
            assert_eq!(ResampleKernel::new(source, target).radius(), expected);
        }
    }
}
