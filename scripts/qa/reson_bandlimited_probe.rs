// Standalone qualification for the exact Reson coefficient and convolution code.
#[path = "../../crates/reson/src/source/resample_kernel.rs"]
mod resample_kernel;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../crates/reson/src/test_support.rs"]
mod test_support;

use resample_kernel::ResampleKernel;
use std::f64::consts::TAU;

const RATE_PAIRS: [(u32, u32); 6] = [
    (44_100, 48_000),
    (48_000, 44_100),
    (44_100, 96_000),
    (96_000, 44_100),
    (48_000, 96_000),
    (96_000, 48_000),
];

fn sine(source_rate: u32, hz: f64) -> Vec<[f32; 2]> {
    (0..source_rate)
        .map(|frame| {
            let phase = TAU * hz * f64::from(frame) / f64::from(source_rate) + 0.371;
            let sample = phase.sin() as f32;
            [sample, -sample]
        })
        .collect()
}

fn render(
    kernel: &ResampleKernel,
    source_rate: u32,
    target_rate: u32,
    input: &[[f32; 2]],
    output_frame: usize,
) -> [f32; 2] {
    let phase_num = output_frame as u64 * u64::from(source_rate);
    let integer = (phase_num / u64::from(target_rate)) as usize;
    let remainder = (phase_num % u64::from(target_rate)) as u32;
    let start = integer + 1 - kernel.radius();
    kernel.render(&input[start..start + 2 * kernel.radius()], remainder)
}

fn measure_tone(
    kernel: &ResampleKernel,
    source_rate: u32,
    target_rate: u32,
    hz: f64,
) -> (f64, f64, f64) {
    let input = sine(source_rate, hz);
    let count = target_rate as usize / 20;
    let skip = target_rate as usize / 10;
    let (mut power, mut sine_sum, mut cosine_sum) = (0.0, 0.0, 0.0);
    for frame in skip..skip + count {
        let [left, right] = render(kernel, source_rate, target_rate, &input, frame);
        assert_eq!(left, -right);
        let sample = f64::from(left);
        let phase = TAU * hz * frame as f64 / f64::from(target_rate);
        power += sample * sample;
        sine_sum += sample * phase.sin();
        cosine_sum += sample * phase.cos();
    }
    let amplitude = 2.0 * sine_sum.hypot(cosine_sum) / count as f64;
    let total = (2.0 * power / count as f64).sqrt();
    let residual = (total * total - amplitude * amplitude).max(0.0).sqrt();
    (db(amplitude), db(residual), db(total))
}

fn db(value: f64) -> f64 {
    20.0 * value.log10()
}

fn spectral_limits(kernel: &ResampleKernel, source_rate: u32, target_rate: u32) -> (f64, f64, f64) {
    let lower_nyquist = f64::from(source_rate.min(target_rate)) / 2.0;
    let upper_nyquist = f64::from(source_rate.max(target_rate)) / 2.0;
    let (mut deviation, mut residual, mut stop) = (0.0_f64, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for index in 1..=16 {
        let pass_hz = (0.85 * lower_nyquist * f64::from(index) / 320.0).floor() * 20.0;
        let (gain, error, _) = measure_tone(kernel, source_rate, target_rate, pass_hz);
        deviation = deviation.max(gain.abs());
        residual = residual.max(error);
        if source_rate > target_rate {
            let hz = lower_nyquist + (upper_nyquist - lower_nyquist) * f64::from(index - 1) / 16.0;
            let stop_hz = (hz / 20.0).ceil() * 20.0;
            stop = stop.max(measure_tone(kernel, source_rate, target_rate, stop_hz).2);
        }
    }
    (deviation, residual, stop)
}

#[cfg(not(test))]
fn main() {
    use std::{hint::black_box, time::Instant};

    for (source_rate, target_rate) in RATE_PAIRS.into_iter().chain([(48_001, 44_100)]) {
        let mut prep = [0.0; 20];
        for elapsed in &mut prep {
            let start = Instant::now();
            let kernel = ResampleKernel::new(source_rate, target_rate);
            *elapsed = start.elapsed().as_secs_f64() * 1_000.0;
            black_box(&kernel);
        }
        prep.sort_by(f64::total_cmp);
        let kernel = ResampleKernel::new(source_rate, target_rate);
        let (deviation, residual, stop) = spectral_limits(&kernel, source_rate, target_rate);
        let input = sine(source_rate, 1_000.0);
        let start_frame = target_rate as usize / 20;
        let end_frame = target_rate as usize * 19 / 20;
        let mut cpu = [0.0; 20];
        for elapsed in &mut cpu {
            let start = Instant::now();
            for frame in start_frame..end_frame {
                black_box(render(&kernel, source_rate, target_rate, &input, frame));
            }
            *elapsed = start.elapsed().as_secs_f64() * 1_000.0 / 0.9;
        }
        cpu.sort_by(f64::total_cmp);
        println!(
            "{source_rate}->{target_rate} taps={} deviation={deviation:.5} dB residual={residual:.2} dB stop={stop:.2} dB prep_p95={:.3} ms render_p95={:.3} ms per 1 s",
            2 * kernel.radius(),
            prep[18],
            cpu[18]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_meets_spectral_targets_across_common_and_interpolated_pairs() {
        for (source_rate, target_rate) in RATE_PAIRS.into_iter().chain([(48_001, 44_100)]) {
            let kernel = ResampleKernel::new(source_rate, target_rate);
            let (deviation, residual, stop) = spectral_limits(&kernel, source_rate, target_rate);
            assert!(
                deviation < 0.05 && residual < -75.0 && stop <= -80.0,
                "{source_rate}->{target_rate} deviation {deviation}, residual {residual}, stop {stop}"
            );
        }
    }

    #[test]
    fn interpolated_phase_wrap_is_continuous_and_rendering_has_no_heap_traffic() {
        let kernel = ResampleKernel::new(48_001, 44_100);
        let history: Vec<_> = (0..=2 * kernel.radius())
            .map(|frame| [frame as f32 / 256.0, 0.0])
            .collect();
        let counts = test_support::count_thread_allocations_and_deallocations(|| {
            let before = kernel.render(&history[..history.len() - 1], 44_099);
            let after = kernel.render(&history[1..], 0);
            assert!((before[0] - after[0]).abs() < 1e-6);
            assert_eq!(before[1], 0.0);
            assert_eq!(after[1], 0.0);
        });
        assert_eq!(counts, (0, 0));
    }
}
