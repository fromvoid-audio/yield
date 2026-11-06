// Standalone qualification for the exact Reson filter and streaming state.
#[path = "../../crates/reson/src/source/resample_kernel.rs"]
mod resample_kernel;
#[path = "../../crates/reson/src/source/streaming_resampler.rs"]
mod streaming_resampler;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../crates/reson/src/test_support.rs"]
mod test_support;

#[cfg(test)]
use resample_kernel::ResampleKernel;
use std::f64::consts::TAU;
use streaming_resampler::StreamingResampler;

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

fn measure_tone(source_rate: u32, target_rate: u32, hz: f64) -> (f64, f64, f64) {
    let input = sine(source_rate, hz);
    let stream = StreamingResampler::new(input.into_iter(), source_rate, target_rate).unwrap();
    let count = target_rate as usize / 20;
    let skip = target_rate as usize / 10;
    let (mut power, mut sine_sum, mut cosine_sum) = (0.0, 0.0, 0.0);
    for (frame, [left, right]) in stream.enumerate().skip(skip).take(count) {
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

fn spectral_limits(source_rate: u32, target_rate: u32) -> (f64, f64, f64) {
    let lower_nyquist = f64::from(source_rate.min(target_rate)) / 2.0;
    let upper_nyquist = f64::from(source_rate.max(target_rate)) / 2.0;
    let (mut deviation, mut residual, mut stop) = (0.0_f64, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for index in 1..=16 {
        let pass_hz = (0.85 * lower_nyquist * f64::from(index) / 320.0).floor() * 20.0;
        let (gain, error, _) = measure_tone(source_rate, target_rate, pass_hz);
        deviation = deviation.max(gain.abs());
        residual = residual.max(error);
        if source_rate > target_rate {
            let hz = lower_nyquist + (upper_nyquist - lower_nyquist) * f64::from(index - 1) / 16.0;
            let stop_hz = (hz / 20.0).ceil() * 20.0;
            stop = stop.max(measure_tone(source_rate, target_rate, stop_hz).2);
        }
    }
    (deviation, residual, stop)
}

#[cfg(not(test))]
fn main() {
    use std::{hint::black_box, time::Instant};

    for (source_rate, target_rate) in RATE_PAIRS.into_iter().chain([(48_001, 44_100)]) {
        let input = sine(source_rate, 1_000.0);
        let (deviation, residual, stop) = spectral_limits(source_rate, target_rate);
        let mut prep = [0.0; 20];
        let mut cpu = [0.0; 20];
        for (preparation, rendering) in prep.iter_mut().zip(&mut cpu) {
            let start = Instant::now();
            let mut stream =
                StreamingResampler::new(input.iter().copied(), source_rate, target_rate).unwrap();
            *preparation = start.elapsed().as_secs_f64() * 1_000.0;
            let start = Instant::now();
            let mut count = 0;
            for frame in stream.by_ref() {
                black_box(frame);
                count += 1;
            }
            *rendering = start.elapsed().as_secs_f64() * 1_000.0;
            assert_eq!(count, target_rate);
            assert_eq!(stream.next_source_position(), (u64::from(source_rate), 0));
            assert_eq!(stream.input().len(), 0);
        }
        prep.sort_by(f64::total_cmp);
        cpu.sort_by(f64::total_cmp);
        println!(
            "{source_rate}->{target_rate} deviation={deviation:.5} dB residual={residual:.2} dB stop={stop:.2} dB prep_p95={:.3} ms stream_p95={:.3} ms per 1 s",
            prep[18], cpu[18]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_meets_spectral_targets_across_common_and_interpolated_pairs() {
        for (source_rate, target_rate) in RATE_PAIRS.into_iter().chain([(48_001, 44_100)]) {
            let (deviation, residual, stop) = spectral_limits(source_rate, target_rate);
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

    #[test]
    fn finite_streams_hold_edges_and_end_at_the_exact_output_frame() {
        for (source_rate, target_rate) in RATE_PAIRS {
            for input_frames in [0_u64, 1, 7, 997] {
                let input = std::iter::repeat_n([0.75_f32, -0.25], input_frames as usize);
                let mut stream = StreamingResampler::new(input, source_rate, target_rate).unwrap();
                let mut rendered = 0;
                let counts = test_support::count_thread_allocations_and_deallocations(|| {
                    for [left, right] in stream.by_ref() {
                        assert!((left - 0.75).abs() < 1e-6);
                        assert!((right + 0.25).abs() < 1e-6);
                        rendered += 1;
                    }
                    assert_eq!(stream.next(), None);
                });
                let expected =
                    (input_frames * u64::from(target_rate)).div_ceil(u64::from(source_rate));
                assert_eq!(rendered, expected);
                assert_eq!(counts, (0, 0));
            }
        }
        assert!(StreamingResampler::new(std::iter::empty(), 512_000, 8_000).is_some());
        assert!(StreamingResampler::new(std::iter::empty(), 512_001, 8_000).is_none());
    }

    #[test]
    fn output_coordinate_and_impulse_alignment_exclude_lookahead() {
        let reads = std::cell::Cell::new(0);
        let input = (0..441).map(|frame| {
            reads.set(reads.get() + 1);
            let amplitude = if frame == 147 { 1.0 } else { 0.0 };
            [amplitude, 0.0]
        });
        let mut stream = StreamingResampler::new(input, 44_100, 48_000).unwrap();
        assert_eq!(reads.get(), 0);
        assert_eq!(stream.next_source_position(), (0, 0));
        assert_eq!(stream.next(), Some([0.0, 0.0]));
        assert_eq!(reads.get(), 81);
        assert_eq!(stream.next_source_position(), (0, 44_100));

        let mut peak = (0, 0.0_f32);
        for (frame, [left, right]) in stream.by_ref().enumerate() {
            assert_eq!(right, 0.0);
            if left.abs() > peak.1 {
                peak = (frame + 1, left.abs());
            }
        }
        assert_eq!(peak.0, 160);
        assert!(peak.1 > 0.9);
        assert_eq!(stream.next_source_position(), (441, 0));
        assert_eq!(reads.get(), 441);
    }

    #[test]
    fn contiguous_loops_preserve_history_and_fractional_phase() {
        let loop_frames = (0..147).map(|frame| {
            let sample = (TAU * f64::from(frame) / 147.0).sin() as f32;
            [sample, -sample]
        });
        let mut stream = StreamingResampler::new(loop_frames.cycle(), 44_100, 48_000).unwrap();
        let mut cycle = [[0.0_f32; 2]; 160];
        let counts = test_support::count_thread_allocations_and_deallocations(|| {
            for _ in 0..160 {
                stream.next().unwrap();
            }
            assert_eq!(stream.next_source_position(), (147, 0));
            for sample in &mut cycle {
                *sample = stream.next().unwrap();
            }
            assert_eq!(stream.next_source_position(), (294, 0));
            for sample in cycle {
                assert_eq!(stream.next(), Some(sample));
            }
        });
        assert_eq!(counts, (0, 0));
    }
}
