//! Explicit-target preparation of a decoded audition span on a background worker.

use std::{ops::Range, sync::Arc, sync::atomic::AtomicBool, time::Instant};

use crate::time_stretch::{StretchPreparationError, Wsola, preparation_checkpoint};

/// Tempo captured when an audition is requested.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempoAuditionTarget {
    /// Use ordinary raw playback.
    Disabled,
    /// Try to match a supplied tempo.
    Enabled {
        /// BPM metadata of the source span.
        source_bpm: Option<f64>,
        /// Target BPM supplied by the audition caller.
        target_bpm: Option<f64>,
    },
}

/// Why the audition should use its ordinary raw playback path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TempoAuditionSkip {
    /// The caller selected raw playback.
    Disabled,
    /// Source or target BPM was unavailable.
    MissingTempo,
    /// Source or target BPM was nonpositive or non-finite.
    InvalidTempo,
    /// The declared source span did not match the PCM layout.
    InvalidSourceSpan,
    /// The PCM could not be prepared within the stretch contract.
    Preparation(StretchPreparationError),
}

/// Exact endpoints for translating cursor and loop coordinates between timelines.
///
/// Interior coordinates use proportional mapping rounded to the nearest frame.
/// The waveform-similarity search may shift individual events within that timeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TempoAuditionFrameMap {
    source: Range<u64>,
    output_frames: u64,
}

impl TempoAuditionFrameMap {
    /// Original half-open frame range in the source file.
    pub fn source_span(&self) -> Range<u64> {
        self.source.clone()
    }

    /// Number of frames in the prepared buffer.
    pub fn output_frames(&self) -> u64 {
        self.output_frames
    }

    /// Map an absolute source boundary to an output-relative boundary, clamping to the span.
    pub fn output_frame_for_source(&self, frame: u64) -> u64 {
        let offset = frame.clamp(self.source.start, self.source.end) - self.source.start;
        scale_frame(
            offset,
            self.source.end - self.source.start,
            self.output_frames,
        )
    }

    /// Map an output-relative boundary to an absolute source boundary, clamping to the span.
    pub fn source_frame_for_output(&self, frame: u64) -> u64 {
        self.source.start
            + scale_frame(
                frame.min(self.output_frames),
                self.output_frames,
                self.source.end - self.source.start,
            )
    }
}

fn scale_frame(frame: u64, from: u64, to: u64) -> u64 {
    let numerator = u128::from(frame) * u128::from(to) + u128::from(from / 2);
    (numerator / u128::from(from)) as u64
}

/// Immutable interleaved PCM ready for playback at the original sample rate and channel layout.
#[derive(Debug)]
pub struct PreparedTempoAudition {
    /// Prepared samples in source channel order.
    pub samples: Arc<[f32]>,
    /// Original and emitted playback coordinates.
    pub frames: TempoAuditionFrameMap,
    /// Source sample rate in hertz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: usize,
}

/// Prepare exactly the decoded source span. The deadline starts at caller admission.
///
/// Run this on a worker and keep the raw source available for fallback. A cancelled request
/// should be discarded by its owner.
pub fn prepare_tempo_audition(
    target: TempoAuditionTarget,
    source_span: Range<u64>,
    input: &[f32],
    sample_rate: u32,
    channels: usize,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<PreparedTempoAudition, TempoAuditionSkip> {
    let ratio = match target {
        TempoAuditionTarget::Disabled => return Err(TempoAuditionSkip::Disabled),
        TempoAuditionTarget::Enabled {
            source_bpm: Some(source),
            target_bpm: Some(target),
        } => {
            if !source.is_finite() || source <= 0.0 || !target.is_finite() || target <= 0.0 {
                return Err(TempoAuditionSkip::InvalidTempo);
            }
            target / source
        }
        TempoAuditionTarget::Enabled { .. } => return Err(TempoAuditionSkip::MissingTempo),
    };
    preparation_checkpoint(cancel, deadline).map_err(TempoAuditionSkip::Preparation)?;
    if channels == 0
        || !input.len().is_multiple_of(channels)
        || source_span.start >= source_span.end
        || source_span.end - source_span.start != (input.len() / channels) as u64
    {
        return Err(TempoAuditionSkip::InvalidSourceSpan);
    }
    let prepared = Wsola::new(sample_rate)
        .prepare_audition(input, channels, ratio, cancel, deadline)
        .map_err(TempoAuditionSkip::Preparation)?;
    let samples = Arc::from(prepared.samples);
    preparation_checkpoint(cancel, deadline).map_err(TempoAuditionSkip::Preparation)?;
    Ok(PreparedTempoAudition {
        samples,
        frames: TempoAuditionFrameMap {
            source: source_span,
            output_frames: prepared.output_frames as u64,
        },
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TARGET: TempoAuditionTarget = TempoAuditionTarget::Enabled {
        source_bpm: Some(100.0),
        target_bpm: Some(150.0),
    };

    #[test]
    fn tempo_audition_prepares_exact_pcm_and_source_coordinates() {
        let input = [0.25, -0.125].repeat(1_201);
        let start = u64::MAX - 1_201;
        let prepared = prepare_tempo_audition(
            TARGET,
            start..u64::MAX,
            &input,
            12_000,
            2,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap();

        assert_eq!((prepared.sample_rate, prepared.channels), (12_000, 2));
        let frames = prepared.frames;
        assert_eq!(frames.source_span(), start..u64::MAX);
        assert_eq!(frames.output_frames(), 801);
        assert_eq!(prepared.samples.len(), 1_602);
        assert_eq!(frames.output_frame_for_source(start - 1), 0);
        assert_eq!(frames.output_frame_for_source(start + 601), 401);
        assert_eq!(frames.output_frame_for_source(u64::MAX), 801);
        assert_eq!(frames.source_frame_for_output(0), start);
        assert_eq!(frames.source_frame_for_output(401), start + 601);
        assert_eq!(frames.source_frame_for_output(801), u64::MAX);
        assert_eq!(frames.source_frame_for_output(802), u64::MAX);
        assert_eq!(&prepared.samples[1_600..], &[0.25, -0.125]);
    }

    #[test]
    fn tempo_audition_classifies_raw_fallbacks_and_cancellation() {
        let input = [0.25; 1_000];
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(500);
        let prepare = |target, range| {
            prepare_tempo_audition(target, range, &input, 12_000, 1, &cancel, deadline).unwrap_err()
        };
        assert_eq!(
            prepare(TempoAuditionTarget::Disabled, 0..1_000),
            TempoAuditionSkip::Disabled
        );
        for (source_bpm, target_bpm, expected) in [
            (None, Some(120.0), TempoAuditionSkip::MissingTempo),
            (Some(100.0), None, TempoAuditionSkip::MissingTempo),
            (Some(f64::NAN), Some(120.0), TempoAuditionSkip::InvalidTempo),
            (Some(100.0), Some(0.0), TempoAuditionSkip::InvalidTempo),
            (
                Some(100.0),
                Some(300.0),
                TempoAuditionSkip::Preparation(StretchPreparationError::UnsupportedRatio),
            ),
        ] {
            assert_eq!(
                prepare(
                    TempoAuditionTarget::Enabled {
                        source_bpm,
                        target_bpm
                    },
                    0..1_000,
                ),
                expected
            );
        }
        assert_eq!(
            prepare(TARGET, 100..1_000),
            TempoAuditionSkip::InvalidSourceSpan
        );
        assert_eq!(
            prepare_tempo_audition(TARGET, 0..1_000, &input, 12_000, 1, &cancel, Instant::now())
                .unwrap_err(),
            TempoAuditionSkip::Preparation(StretchPreparationError::DeadlineExceeded)
        );
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            prepare(TARGET, 0..1_000),
            TempoAuditionSkip::Preparation(StretchPreparationError::Canceled)
        );
    }
}
