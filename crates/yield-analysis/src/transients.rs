use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Complex32, FftPlan, WavPcm, fft_radix2_inplace_with_plan, hann_window};

const BAND_COUNT: usize = 24;
const FLUX_HISTORY: usize = 5;
const MAX_WINDOWS: usize = 32_000;
/// Maximum number of frame indices returned for one analyzed source.
pub const MAX_TRANSIENT_MARKERS: usize = 2_048;

/// Version of the detector and its preset thresholds for persisted result identity.
pub const DETECTOR_VERSION: &str = "native-spectral-flux-v1";

/// Sensitivity applied to the same native PCM novelty curve.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TransientPreset {
    /// Favor strong attacks.
    Low,
    /// Show typical note and percussion attacks.
    #[default]
    Normal,
    /// Include weaker attacks.
    High,
}

impl TransientPreset {
    /// Stable key scoped to the detector version.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    fn threshold(self, baseline: f32) -> f32 {
        let (multiplier, floor) = match self {
            Self::Low => (6.0, 0.5),
            Self::Normal => (4.0, 0.25),
            Self::High => (2.5, 0.08),
        };
        (baseline * multiplier).max(floor)
    }
}

/// Bounded, ordered onset positions for one complete native PCM input.
#[derive(Debug)]
pub struct TransientMarkers {
    frames: Vec<u32>,
    source_frames: u32,
    preset: TransientPreset,
}

impl TransientMarkers {
    /// Zero-based positions in the original audio, ordered and unique.
    pub fn frames(&self) -> &[u32] {
        &self.frames
    }

    /// Number of frames in the analyzed input, including any trailing silence.
    pub fn source_frames(&self) -> u32 {
        self.source_frames
    }

    /// Preset used to pick the markers.
    pub fn preset(&self) -> TransientPreset {
        self.preset
    }
}

/// Detect attacks using spectral flux and refine each peak against the original audio.
///
/// Work is limited by the PCM decoder's sample bound and the detector's window
/// and marker caps. Stereo channels contribute spectral magnitudes independently.
pub fn detect_transients(
    pcm: &WavPcm,
    preset: TransientPreset,
    cancel: &AtomicBool,
) -> Result<TransientMarkers, &'static str> {
    check_cancelled(cancel)?;
    let frame_count = pcm.samples().len() / usize::from(pcm.channels());
    let fft_len = match pcm.sample_rate() {
        0..3_000 => 128,
        3_000..6_000 => 256,
        6_000..32_000 => 512,
        _ => 1_024,
    };
    let hop = (fft_len / 4).max(frame_count.div_ceil(MAX_WINDOWS));
    let mut flux = SpectralFlux::new(fft_len, pcm.sample_rate())?;
    let mut novelty = Vec::with_capacity(frame_count.div_ceil(hop));
    for center in (0..frame_count).step_by(hop) {
        check_cancelled(cancel)?;
        novelty.push(flux.measure(pcm, center)?);
    }

    let mut sorted = novelty.clone();
    let middle = sorted.len() / 2;
    let (_, baseline, _) = sorted.select_nth_unstable_by(middle, f32::total_cmp);
    let threshold = preset.threshold(*baseline);
    let min_gap = (pcm.sample_rate() as usize * 35)
        .div_ceil(1_000)
        .max(frame_count.div_ceil(MAX_TRANSIENT_MARKERS));
    let mut frames = Vec::new();
    for (index, &score) in novelty.iter().enumerate() {
        if index % 256 == 0 {
            check_cancelled(cancel)?;
        }
        let previous = index.checked_sub(1).map_or(0.0, |i| novelty[i]);
        let next = novelty.get(index + 1).copied().unwrap_or(0.0);
        if score < threshold || score <= previous || score < next {
            continue;
        }
        let center = index * hop;
        let start = center.saturating_sub(fft_len / 2);
        let end = (center + fft_len / 2).min(frame_count);
        if let Some(frame) = refine_attack(pcm, start, end)
            && frames
                .last()
                .is_none_or(|&previous| frame >= previous as usize + min_gap)
        {
            frames.push(frame as u32);
        }
    }
    check_cancelled(cancel)?;
    Ok(TransientMarkers {
        frames,
        source_frames: frame_count as u32,
        preset,
    })
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), &'static str> {
    if cancel.load(Ordering::Acquire) {
        return Err("Cancelled");
    }
    Ok(())
}

// The reference Radiant detector uses log-spaced bands, log magnitude, and
// positive novelty above the delayed local maximum. Keep that comparison while
// measuring each channel before combining its magnitudes.
struct SpectralFlux {
    window: Vec<f32>,
    fft: FftPlan,
    buffer: Vec<Complex32>,
    bands: Vec<std::ops::Range<usize>>,
    history: [[f32; FLUX_HISTORY]; BAND_COUNT],
    cursor: usize,
}

impl SpectralFlux {
    fn new(fft_len: usize, sample_rate: u32) -> Result<Self, &'static str> {
        Ok(Self {
            window: hann_window(fft_len),
            fft: FftPlan::new(fft_len).map_err(|_| "Invalid transient FFT")?,
            buffer: vec![Complex32::default(); fft_len],
            bands: bands(fft_len, sample_rate),
            history: [[0.0; FLUX_HISTORY]; BAND_COUNT],
            cursor: 0,
        })
    }

    fn measure(&mut self, pcm: &WavPcm, center: usize) -> Result<f32, &'static str> {
        let channels = usize::from(pcm.channels());
        let start = center as isize - (self.window.len() / 2) as isize;
        let mut magnitudes = [0.0_f32; BAND_COUNT];
        for channel in 0..channels {
            for (index, (bin, &weight)) in self.buffer.iter_mut().zip(&self.window).enumerate() {
                let sample = usize::try_from(start + index as isize)
                    .ok()
                    .and_then(|frame| pcm.samples().get(frame * channels + channel))
                    .copied()
                    .map_or(0.0, finite_sample);
                *bin = Complex32::new(sample * weight, 0.0);
            }
            fft_radix2_inplace_with_plan(&mut self.buffer, &self.fft)
                .map_err(|_| "Invalid transient FFT")?;
            for (range, maximum) in self.bands.iter().zip(&mut magnitudes) {
                let sum = self.buffer[range.clone()]
                    .iter()
                    .map(|bin| (10.0 * bin.norm()).ln_1p())
                    .sum::<f32>();
                *maximum = maximum.max(sum / range.len() as f32);
            }
        }
        let mut novelty = 0.0;
        for (index, &value) in magnitudes.iter().take(self.bands.len()).enumerate() {
            let history = &mut self.history[index];
            let previous = (2..FLUX_HISTORY)
                .map(|offset| history[(self.cursor + FLUX_HISTORY - offset) % FLUX_HISTORY])
                .fold(0.0, f32::max);
            let weight = ((index + 1) as f32 / self.bands.len() as f32).sqrt();
            novelty += (value - previous).max(0.0) * weight;
            history[self.cursor] = value;
        }
        self.cursor = (self.cursor + 1) % FLUX_HISTORY;
        Ok(novelty / self.bands.len() as f32)
    }
}

fn bands(fft_len: usize, sample_rate: u32) -> Vec<std::ops::Range<usize>> {
    let last_bin = fft_len / 2;
    let nyquist = sample_rate as f32 / 2.0;
    let min_hz = 40.0_f32.min(nyquist / 2.0);
    let octave_span = nyquist / min_hz;
    let mut start = 1;
    let mut ranges = Vec::with_capacity(BAND_COUNT);
    for band in 1..=BAND_COUNT {
        let edge_hz = min_hz * octave_span.powf(band as f32 / BAND_COUNT as f32);
        let end = (edge_hz / nyquist * last_bin as f32).round() as usize + 1;
        if end > start {
            ranges.push(start..end);
            start = end;
        }
    }
    ranges
}

fn refine_attack(pcm: &WavPcm, start: usize, end: usize) -> Option<usize> {
    let channels = usize::from(pcm.channels());
    let audio = pcm.samples();
    let mut best = None;
    let mut strongest = 0.0;
    for frame in start..end {
        let current = frame * channels;
        for channel in 0..channels {
            let sample = finite_sample(audio[current + channel]).abs();
            let previous = current
                .checked_sub(channels)
                .map_or(0.0, |offset| finite_sample(audio[offset + channel]).abs());
            let jump = sample - previous;
            if jump > strongest {
                strongest = jump;
                best = Some(frame);
            }
        }
    }
    best
}

fn finite_sample(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn decode(channels: u16, sample_rate: u32, samples: &[f32]) -> WavPcm {
        let mut bytes = Vec::new();
        let mut writer = hound::WavWriter::new(
            Cursor::new(&mut bytes),
            hound::WavSpec {
                channels,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        crate::decode_wav_pcm(&bytes, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn resolves_unaligned_mono_and_antiphase_stereo_impulses_to_native_frames() {
        let cancel = AtomicBool::new(false);
        let mut mono = vec![0.0; 16_000];
        mono[1237] = 0.8;
        mono[9673] = -0.8;
        let pcm = decode(1, 16_000, &mono);
        let markers = detect_transients(&pcm, TransientPreset::Normal, &cancel).unwrap();
        assert_eq!(markers.source_frames(), 16_000);
        assert_eq!(markers.frames(), [1237, 9673]);

        let mut stereo = vec![0.0; 32_000];
        stereo[1237 * 2..1237 * 2 + 2].copy_from_slice(&[0.8, -0.8]);
        stereo[9673 * 2 + 1] = 0.8;
        let pcm = decode(2, 16_000, &stereo);
        let markers = detect_transients(&pcm, TransientPreset::Normal, &cancel).unwrap();
        assert_eq!(markers.frames(), [1237, 9673]);
    }

    #[test]
    fn presets_select_distinct_attacks() {
        let cancel = AtomicBool::new(false);
        let mut mono = vec![0.0; 16_000];
        mono[2000] = 0.025;
        mono[7000] = 0.06;
        mono[12000] = 0.8;
        let pcm = decode(1, 16_000, &mono);
        for (preset, expected) in [
            (TransientPreset::Low, &[12000][..]),
            (TransientPreset::Normal, &[7000, 12000]),
            (TransientPreset::High, &[2000, 7000, 12000]),
        ] {
            let markers = detect_transients(&pcm, preset, &cancel).unwrap();
            assert_eq!(markers.preset(), preset);
            assert_eq!(markers.frames(), expected, "{preset:?}");
        }
    }

    #[test]
    fn silence_is_empty_and_cancellation_does_not_return_markers() {
        let pcm = decode(1, 48_000, &[0.0; 4096]);
        for preset in [
            TransientPreset::Low,
            TransientPreset::Normal,
            TransientPreset::High,
        ] {
            let markers = detect_transients(&pcm, preset, &AtomicBool::new(false)).unwrap();
            assert!(markers.frames().is_empty());
        }
        assert_eq!(
            detect_transients(&pcm, TransientPreset::Normal, &AtomicBool::new(true)).unwrap_err(),
            "Cancelled"
        );
    }

    #[test]
    fn centered_windows_find_attacks_at_both_recording_edges() {
        let mut mono = vec![0.0; 4096];
        mono[0] = 0.8;
        mono[4095] = -0.8;
        let pcm = decode(1, 48_000, &mono);
        let markers =
            detect_transients(&pcm, TransientPreset::Normal, &AtomicBool::new(false)).unwrap();
        assert_eq!(markers.frames(), [0, 4095]);
    }

    #[test]
    fn finds_an_attack_over_a_sustained_tone_and_quiet_noise() {
        let mut seed = 1_u32;
        let mut mono = (0..16_000)
            .map(|index| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed as f32 / u32::MAX as f32 - 0.5) * 0.005;
                let tone = (index as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin();
                tone * 0.05 + noise
            })
            .collect::<Vec<_>>();
        mono[8013] = 0.8;
        let pcm = decode(1, 16_000, &mono);
        let markers =
            detect_transients(&pcm, TransientPreset::High, &AtomicBool::new(false)).unwrap();
        let interior = markers
            .frames()
            .iter()
            .copied()
            .filter(|frame| (512..15_488).contains(frame))
            .collect::<Vec<_>>();
        assert_eq!(interior, [8013]);
    }

    #[test]
    fn dense_long_input_keeps_native_positions_bounded() {
        let mut samples = vec![0.0; 256 * (MAX_TRANSIENT_MARKERS + 32)];
        for sample in samples.iter_mut().skip(65).step_by(256) {
            *sample = 0.8;
        }
        let pcm = decode(1, 1000, &samples);
        let markers =
            detect_transients(&pcm, TransientPreset::High, &AtomicBool::new(false)).unwrap();
        assert!(markers.frames().len() <= MAX_TRANSIENT_MARKERS);
        assert!(markers.frames().len() > MAX_TRANSIENT_MARKERS / 2);
        assert!(markers.frames().windows(2).all(|pair| pair[0] < pair[1]));
        assert!(markers.frames().iter().all(|&frame| frame % 256 == 65));
    }
}
