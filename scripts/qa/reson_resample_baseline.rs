// Compare the old adapter using the checkout procedure in docs/RESON_RESAMPLING.md.
#![allow(dead_code)]
mod output_adapter;
use output_adapter::{OutputAdapter, StereoOutputPair};
use std::{
    f64::consts::TAU,
    hint::black_box,
    time::{Duration, Instant},
};
trait Source: Iterator<Item = f32> + Send {
    fn current_frame_len(&self) -> Option<usize>;
    fn channels(&self) -> u16;
    fn sample_rate(&self) -> u32;
    fn total_duration(&self) -> Option<Duration>;
    fn last_error(&self) -> Option<String> {
        None
    }
}
struct Pcm<'a> {
    rate: u32,
    input: std::slice::Iter<'a, f32>,
}
impl Iterator for Pcm<'_> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        self.input.next().copied()
    }
}
impl Source for Pcm<'_> {
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.input.len())
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
fn pcm(source: u32, hz: f64) -> Vec<f32> {
    (0..source)
        .flat_map(|i| {
            let x = (TAU * hz * f64::from(i) / f64::from(source)).sin() as f32;
            [x, -x]
        })
        .collect()
}
fn measure(src: u32, dst: u32, hz: f64) -> (f64, f64, f64) {
    let input = pcm(src, hz);
    let mut out = OutputAdapter::with_output_pair(
        Pcm {
            rate: src,
            input: input.iter(),
        },
        dst,
        2,
        StereoOutputPair::default(),
    );
    let mut norm = 0.0;
    let mut sin = 0.0;
    let mut cos = 0.0;
    let n = dst as usize / 2;
    for (frame, x) in out
        .by_ref()
        .step_by(2)
        .enumerate()
        .skip(dst as usize / 10)
        .take(n)
    {
        let x = f64::from(x);
        let phase = TAU * hz * frame as f64 / f64::from(dst);
        norm += x * x;
        sin += x * phase.sin();
        cos += x * phase.cos();
    }
    let fundamental = (sin.hypot(cos)) * 2.0 / n as f64;
    let total = (norm * 2.0 / n as f64).sqrt();
    let residual = (total * total - fundamental * fundamental).max(0.0).sqrt();
    (
        20.0 * fundamental.log10(),
        20.0 * residual.log10(),
        20.0 * total.log10(),
    )
}
fn main() {
    for (src, dst) in [
        (44100, 48000),
        (48000, 44100),
        (44100, 96000),
        (96000, 44100),
        (48000, 96000),
        (96000, 48000),
    ] {
        let lower = f64::from(src.min(dst)) * 0.5;
        let upper = f64::from(src.max(dst)) * 0.5;
        let hz = (lower * 0.85 / 10.0).floor() * 10.0;
        let (gain, image, _) = measure(src, dst, hz);
        let stop =
            (src > dst).then(|| measure(src, dst, ((lower + upper) * 0.05).floor() * 10.0).2);
        let input = pcm(src, 1000.0);
        let mut times = Vec::new();
        for _ in 0..12 {
            let start = Instant::now();
            let mut out = OutputAdapter::with_output_pair(
                Pcm {
                    rate: src,
                    input: input.iter(),
                },
                dst,
                2,
                StereoOutputPair::default(),
            );
            let mut samples = 0;
            while let Some(x) = out.next() {
                black_box(x);
                samples += 1;
            }
            assert_eq!(samples, dst * 2);
            times.push(start.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{src}->{dst} pass_hz={hz} pass_gain_db={gain:.3} residual_db={image:.3} stop_db={stop:?} median_ms={:.3}",
            times[6]
        );
    }
}
