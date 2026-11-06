//! Time-stretch helpers for BPM-synced playback.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const MIN_STRETCH_RATIO: f64 = 0.5;
const MAX_STRETCH_RATIO: f64 = 2.0;
const UNITY_TOLERANCE: f64 = 1e-3;
const SILENCE_ENERGY: f32 = 1e-6;
const SIMILARITY_THRESHOLD: f32 = 0.2;

/// Maximum interleaved PCM samples admitted by tempo audition.
pub const MAX_TEMPO_INPUT_SAMPLES: usize = 1_048_576;

/// Maximum source duration in seconds admitted by tempo audition.
pub const MAX_TEMPO_SPAN_SECONDS: u32 = 8;

/// Result of preparing a bounded tempo audition span.
#[derive(Debug)]
pub struct PreparedTempoStretch {
    /// Interleaved PCM at the original sample rate and channel layout.
    pub samples: Vec<f32>,
    /// Exact number of source frames.
    pub source_frames: usize,
    /// Exact number of prepared playback frames.
    pub output_frames: usize,
}

/// Why a bounded audition span could not be stretched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StretchPreparationError {
    /// PCM is empty, non-finite, or has invalid frame layout.
    InvalidPcm,
    /// The input exceeds the duration or sample budget.
    InputTooLarge,
    /// The target/source tempo ratio is outside the supported range.
    UnsupportedRatio,
    /// The tempo is effectively unchanged.
    UnityRatio,
    /// The input is too short for a full overlap.
    InputTooShort,
    /// The owner canceled preparation.
    Canceled,
    /// The worker exceeded its deadline.
    DeadlineExceeded,
}

impl fmt::Display for StretchPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPcm => "invalid tempo audition PCM",
            Self::InputTooLarge => "tempo audition exceeds the input budget",
            Self::UnsupportedRatio => "unsupported tempo audition ratio",
            Self::UnityRatio => "tempo audition already matches the target",
            Self::InputTooShort => "tempo audition span is too short",
            Self::Canceled => "tempo audition preparation canceled",
            Self::DeadlineExceeded => "tempo audition preparation timed out",
        })
    }
}

impl std::error::Error for StretchPreparationError {}

pub(super) fn preparation_checkpoint(
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<(), StretchPreparationError> {
    if cancel.load(Ordering::Acquire) {
        Err(StretchPreparationError::Canceled)
    } else if Instant::now() >= deadline {
        Err(StretchPreparationError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

/// WSOLA time-stretcher for interleaved audition PCM.
pub struct Wsola {
    sample_rate: u32,
    window_size: usize,
    hop: usize,
    search_radius: usize,
    fade_in: Vec<f32>,
}

impl Wsola {
    /// Build a WSOLA helper for a given sample rate.
    pub fn new(sample_rate: u32) -> Self {
        let mut window_size = ((sample_rate.max(1) as f32) * 0.025).round() as usize;
        window_size = window_size.clamp(256, 4096);
        if !window_size.is_multiple_of(2) {
            window_size += 1;
        }
        let hop = window_size / 2;
        let fade_in = (0..hop)
            .map(|frame| {
                let phase = std::f64::consts::PI * frame as f64 / (hop - 1) as f64;
                (0.5 - 0.5 * phase.cos()) as f32
            })
            .collect();
        Self {
            sample_rate,
            window_size,
            hop,
            search_radius: hop / 2,
            fade_in,
        }
    }

    /// Stretch interleaved audio. Unchanged or too-short input returns a copy of the original.
    pub fn stretch(&self, input: &[f32], channels: usize, ratio: f64) -> Vec<f32> {
        let channels = channels.max(1);
        if !ratio.is_finite()
            || (ratio - 1.0).abs() < UNITY_TOLERANCE
            || input.len() / channels < self.window_size * 2
        {
            return input.to_vec();
        }
        self.stretch_inner(
            input,
            channels,
            ratio.clamp(MIN_STRETCH_RATIO, MAX_STRETCH_RATIO),
            || Ok(()),
        )
        .unwrap_or_else(|_| input.to_vec())
    }

    /// Prepare a bounded audition span on a worker. The ratio is target BPM divided by source BPM.
    ///
    /// The caller keeps the original PCM available for raw fallback on any preparation error.
    pub fn prepare_audition(
        &self,
        input: &[f32],
        channels: usize,
        ratio: f64,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<PreparedTempoStretch, StretchPreparationError> {
        let checkpoint = || preparation_checkpoint(cancel, deadline);
        checkpoint()?;
        if channels == 0
            || self.sample_rate == 0
            || input.is_empty()
            || !input.len().is_multiple_of(channels)
        {
            return Err(StretchPreparationError::InvalidPcm);
        }
        let source_frames = input.len() / channels;
        if input.len() > MAX_TEMPO_INPUT_SAMPLES
            || source_frames as u64
                > u64::from(self.sample_rate) * u64::from(MAX_TEMPO_SPAN_SECONDS)
        {
            return Err(StretchPreparationError::InputTooLarge);
        }
        if !(MIN_STRETCH_RATIO..=MAX_STRETCH_RATIO).contains(&ratio) {
            return Err(StretchPreparationError::UnsupportedRatio);
        }
        if (ratio - 1.0).abs() < UNITY_TOLERANCE {
            return Err(StretchPreparationError::UnityRatio);
        }
        if source_frames < self.window_size * 2 {
            return Err(StretchPreparationError::InputTooShort);
        }
        if input.iter().any(|sample| !sample.is_finite()) {
            return Err(StretchPreparationError::InvalidPcm);
        }

        let samples = self.stretch_inner(input, channels, ratio, &checkpoint)?;
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(StretchPreparationError::InvalidPcm);
        }
        checkpoint()?;
        Ok(PreparedTempoStretch {
            output_frames: samples.len() / channels,
            source_frames,
            samples,
        })
    }

    fn stretch_inner(
        &self,
        input: &[f32],
        channels: usize,
        ratio: f64,
        checkpoint: impl Fn() -> Result<(), StretchPreparationError>,
    ) -> Result<Vec<f32>, StretchPreparationError> {
        checkpoint()?;
        let input_frames = input.len() / channels;
        let output_frames = (input_frames as f64 / ratio).round() as usize;
        let output_len = output_frames
            .checked_mul(channels)
            .ok_or(StretchPreparationError::InputTooLarge)?;
        let mut output = vec![0.0; output_len];
        let mut covered = self.window_size.min(output_frames);
        let initial_len = covered * channels;
        output[..initial_len].copy_from_slice(&input[..initial_len]);

        let mut synthesis = self.hop;
        while covered < output_frames {
            checkpoint()?;
            let block_frames = self.window_size.min(output_frames - synthesis);
            let overlap_frames = covered - synthesis;
            let output_start = synthesis * channels;
            let overlap_end = covered * channels;
            let max_source_frame = input_frames - block_frames;
            let expected = ((synthesis as f64 * ratio).round() as usize).min(max_source_frame);
            let source_frame = self.best_overlap(
                input,
                &output[output_start..overlap_end],
                channels,
                expected,
                max_source_frame,
                &checkpoint,
            )?;
            let source_start = source_frame * channels;
            for frame in 0..overlap_frames {
                let fade_in = self.fade_in[frame];
                let offset = frame * channels;
                for channel in 0..channels {
                    let old = &mut output[output_start + offset + channel];
                    let new = input[source_start + offset + channel];
                    *old = *old * (1.0 - fade_in) + new * fade_in;
                }
            }
            covered = synthesis + block_frames;
            let source_overlap_end = source_start + overlap_frames * channels;
            let source_end = source_start + block_frames * channels;
            output[overlap_end..covered * channels]
                .copy_from_slice(&input[source_overlap_end..source_end]);
            synthesis += self.hop;
        }
        Ok(output)
    }

    fn best_overlap(
        &self,
        input: &[f32],
        reference: &[f32],
        channels: usize,
        expected: usize,
        max_source_frame: usize,
        checkpoint: &impl Fn() -> Result<(), StretchPreparationError>,
    ) -> Result<usize, StretchPreparationError> {
        let reference_energy = reference.iter().map(|sample| sample * sample).sum::<f32>();
        if reference_energy <= SILENCE_ENERGY {
            return Ok(expected);
        }
        let first = expected.saturating_sub(self.search_radius);
        let last = (expected + self.search_radius).min(max_source_frame);
        let mut best = expected;
        let mut best_score = SIMILARITY_THRESHOLD;
        for frame in first..=last {
            if (frame - first).is_multiple_of(32) {
                checkpoint()?;
            }
            let start = frame * channels;
            let candidate = &input[start..start + reference.len()];
            let (dot, energy) = reference
                .iter()
                .zip(candidate)
                .fold((0.0, 0.0), |(dot, energy), (left, right)| {
                    (dot + left * right, energy + right * right)
                });
            if energy <= SILENCE_ENERGY {
                continue;
            }
            let score = dot / (reference_energy * energy).sqrt();
            if score > best_score
                || (score == best_score && frame.abs_diff(expected) < best.abs_diff(expected))
            {
                best_score = score;
                best = frame;
            }
        }
        Ok(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn bounded_stretch_keeps_exact_length_and_fills_partial_tail() {
        let sample_rate = 12_000;
        let frames = 2_000;
        let samples: Vec<_> = (0..frames).flat_map(|_| [0.25, -0.125]).collect();
        let wsola = Wsola::new(sample_rate);
        for ratio in [0.5, 0.8, 1.5, 2.0] {
            let prepared = wsola
                .prepare_audition(
                    &samples,
                    2,
                    ratio,
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_millis(500),
                )
                .unwrap();
            let expected = (frames as f64 / ratio).round() as usize;
            assert_eq!(prepared.source_frames, frames);
            assert_eq!(prepared.output_frames, expected);
            assert_eq!(prepared.samples.len(), expected * 2);
            for frame in prepared.samples.chunks_exact(2) {
                assert!((frame[0] - 0.25).abs() < 1e-6);
                assert!((frame[1] + 0.125).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn wsola_preserves_tone_pitch_and_antiphase_stereo() {
        let rate = 12_000;
        let frequency = 400.0;
        let input = (0..6_000)
            .flat_map(|frame| {
                let phase = std::f32::consts::TAU * frequency * frame as f32 / rate as f32;
                let left = phase.sin() * 0.5;
                [left, -left]
            })
            .collect::<Vec<_>>();
        let wsola = Wsola::new(rate);
        for ratio in [0.75, 1.5] {
            let output = wsola.stretch(&input, 2, ratio);
            let frames = output.len() / 2;
            let interior = &output[600..output.len() - 600];
            let crossings = interior
                .chunks_exact(2)
                .map(|frame| {
                    assert!((frame[0] + frame[1]).abs() < 1e-6);
                    frame[0]
                })
                .collect::<Vec<_>>()
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count();
            let measured = crossings as f32 * rate as f32 / (frames - 600) as f32;
            assert!((measured - frequency).abs() < 5.0, "{ratio}: {measured}");
        }
    }

    #[test]
    fn bounded_audition_reports_fallback_reasons_without_partial_pcm() {
        let wsola = Wsola::new(12_000);
        let samples = vec![0.0; 1_000];
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(500);
        for (input, channels, ratio, expected) in [
            (&samples[..], 1, 1.0, StretchPreparationError::UnityRatio),
            (
                &samples[..],
                1,
                f64::NAN,
                StretchPreparationError::UnsupportedRatio,
            ),
            (
                &samples[..],
                1,
                2.5,
                StretchPreparationError::UnsupportedRatio,
            ),
            (&samples[..3], 2, 1.5, StretchPreparationError::InvalidPcm),
            (
                &samples[..4],
                2,
                1.5,
                StretchPreparationError::InputTooShort,
            ),
        ] {
            assert_eq!(
                wsola
                    .prepare_audition(input, channels, ratio, &cancel, deadline)
                    .unwrap_err(),
                expected
            );
        }
        assert_eq!(
            wsola
                .prepare_audition(&samples, 1, 1.5, &cancel, Instant::now())
                .unwrap_err(),
            StretchPreparationError::DeadlineExceeded
        );
        cancel.store(true, Ordering::Release);
        assert_eq!(
            wsola
                .prepare_audition(&samples, 1, 1.5, &cancel, deadline)
                .unwrap_err(),
            StretchPreparationError::Canceled
        );
    }

    #[test]
    fn synthesis_checks_cancellation_during_overlap_search() {
        let wsola = Wsola::new(12_000);
        let checkpoints = Cell::new(0);
        let result = wsola.stretch_inner(&vec![0.25; 2_000], 1, 0.5, || {
            let count = checkpoints.get() + 1;
            checkpoints.set(count);
            if count == 3 {
                Err(StretchPreparationError::Canceled)
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err(), StretchPreparationError::Canceled);
        assert_eq!(checkpoints.get(), 3);
    }

    #[test]
    fn wsola_preserves_silence() {
        let wsola = Wsola::new(44_100);
        let input = vec![0.0; 4_000];
        let output = wsola.stretch(&input, 1, 1.5);
        assert_eq!(output.len(), 2_667);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }
}
