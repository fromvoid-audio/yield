//! Revision-checked preparation of whole-file selection edits, without publication.

use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;

use yield_core::WaveformEditSettings;

use crate::{WavSpanExportRequest, WaveformDetailContext, WaveformDetailRange, detail};

/// Hard ceiling for the interleaved decoded buffer used by edit preparation (256 MiB).
pub const MAX_WAV_EDIT_DECODED_BYTES: usize = 256 * 1024 * 1024;

const NORMALIZED_PEAK_TOLERANCE: f32 = 0.001;

/// Result of preparing a whole-file peak normalization. Skipped results leave the sink empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavNormalizationPreparationOutcome {
    /// The caller-owned sink contains a complete unpublished normalized WAV.
    Prepared,
    /// The source contains no peak above the reference silence threshold.
    SkippedSilent,
    /// The source peak is already within the reference full-scale tolerance.
    SkippedAlreadyNormalized,
}

/// Native selection operations supported by unpublished WAV preparation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WavEditOperation {
    /// Apply gain and inner/outer fades with the reference editor's sample arithmetic.
    GainAndFades(WaveformEditSettings),
    /// Reverse complete selected frames, preserving channel order within each frame.
    Reverse,
    /// Replace selected samples with positive zero while preserving the file length.
    Mute,
    /// Remove selected existing frames, joining the retained prefix and suffix.
    Trim,
    /// Keep only selected existing frames; silence outside the source is not supported here.
    Crop,
    /// Rotate the whole file by signed frames, wrapping at the ends without changing length.
    Slide {
        /// Positive offsets move audio right; negative offsets move it left.
        frame_offset: i64,
    },
    /// Keep a signed half-open frame span, padding portions outside the source with silence.
    CropWithSilence {
        /// Inclusive start frame; negative values select silence before the source.
        start_frame: i64,
        /// Exclusive end frame; values beyond the source select trailing silence.
        end_frame: i64,
    },
}

/// A revision-fenced edit of a half-open selection within a whole WAV file.
#[derive(Clone, Debug, PartialEq)]
pub struct WavEditPreparationRequest {
    /// Source identity, revision and format captured before background admission.
    pub context: WaveformDetailContext,
    /// Existing source frames to edit; outer fades may also change adjacent frames.
    /// For `CropWithSilence` and `Slide`, this must be the whole source.
    pub range: WaveformDetailRange,
    /// Operation to apply to the selected frames.
    pub operation: WavEditOperation,
}

impl WavEditPreparationRequest {
    /// Resolve a waveform edit selection using native floor-start/ceil-end frame coverage.
    ///
    /// `Crop` preserves signed bounds and becomes `CropWithSilence`. Gain/fades, reverse,
    /// mute and trim use only the source intersection and reject wholly absent audio.
    /// Frame-authored crops and whole-file slides use their dedicated constructors instead.
    pub fn from_edit_region(
        context: WaveformDetailContext,
        region: yield_core::WaveformEditRegion,
        operation: WavEditOperation,
    ) -> Result<Self, String> {
        if operation == WavEditOperation::Crop {
            let (start, end) = region
                .signed_frame_bounds(context.total_frames)
                .ok_or_else(|| String::from("crop selection cannot be represented in frames"))?;
            return Self::with_padded_crop(context, start, end);
        }
        if matches!(
            operation,
            WavEditOperation::CropWithSilence { .. } | WavEditOperation::Slide { .. }
        ) {
            return Err(String::from(
                "frame-authored crops and whole-file slides require their dedicated constructors",
            ));
        }
        let (start_frame, end_frame) = region
            .existing_frame_bounds(context.total_frames)
            .ok_or_else(|| {
                String::from("edit selection is outside audio or cannot be represented in frames")
            })?;
        Self::with_operation(
            context,
            WaveformDetailRange {
                start_frame,
                end_frame,
            },
            operation,
        )
    }

    /// Validate the format and exact selection without decoding or writing audio.
    pub fn new(
        context: WaveformDetailContext,
        range: WaveformDetailRange,
        settings: WaveformEditSettings,
    ) -> Result<Self, String> {
        Self::with_operation(context, range, WavEditOperation::GainAndFades(settings))
    }

    /// Validate a selected operation without decoding or writing audio.
    pub fn with_operation(
        context: WaveformDetailContext,
        range: WaveformDetailRange,
        operation: WavEditOperation,
    ) -> Result<Self, String> {
        WavSpanExportRequest::new(context.clone(), range)?;
        if matches!(operation, WavEditOperation::Slide { .. })
            && (range.start_frame != 0 || range.end_frame != context.total_frames)
        {
            return Err(String::from("slide requires the whole source range"));
        }
        if operation == WavEditOperation::Trim && range.len() == context.total_frames {
            return Err(String::from("trimming the entire WAV would leave no audio"));
        }
        if let WavEditOperation::CropWithSilence {
            start_frame,
            end_frame,
        } = operation
        {
            if range.start_frame != 0 || range.end_frame != context.total_frames {
                return Err(String::from(
                    "padded crop requires a whole-source context range",
                ));
            }
            crate::padded_crop::PaddedCropPlan::new(
                context.total_frames,
                usize::from(context.spec.channels),
                start_frame,
                end_frame,
            )?;
        }
        Ok(Self {
            context,
            range,
            operation,
        })
    }

    /// Create a crop with exact signed frame bounds, including silence outside the source.
    ///
    /// Bounds must describe at least one frame. Output size and the decoded-memory budget are
    /// revalidated at execution before allocating or writing. This does not publish a file.
    pub fn with_padded_crop(
        context: WaveformDetailContext,
        start_frame: i64,
        end_frame: i64,
    ) -> Result<Self, String> {
        let range = WaveformDetailRange {
            start_frame: 0,
            end_frame: context.total_frames,
        };
        Self::with_operation(
            context,
            range,
            WavEditOperation::CropWithSilence {
                start_frame,
                end_frame,
            },
        )
    }

    /// Prepare a whole-file cyclic shift in exact frames, preserving all channels and samples.
    pub fn with_slide(context: WaveformDetailContext, frame_offset: i64) -> Result<Self, String> {
        let range = WaveformDetailRange {
            start_frame: 0,
            end_frame: context.total_frames,
        };
        Self::with_operation(context, range, WavEditOperation::Slide { frame_offset })
    }
}

/// Prepare the whole edited WAV in a fresh caller-owned sink, without publishing it.
///
/// `decoded_byte_budget` bounds the decoded f32 buffer, additionally capped by
/// [`MAX_WAV_EDIT_DECODED_BYTES`]. The caller must run this blocking work off the UI thread.
/// Output retains channels and sample rate and uses native unclipped 32-bit float encoding.
/// Trim removes selected frames; crop retains only selected frames. Other operations retain
/// the original frame count. `CropWithSilence` accepts signed bounds and pads absent frames.
/// Other selection bounds must lie within existing audio. The budget bounds the larger of
/// the source and output decoded buffers; one buffer is reserved before decoding.
/// The source is checked before decoding, before output and after finalization. A successful
/// result is only a prepared payload, not a durable file operation or publication authority.
/// Any error requires discarding the output, including a stale-source error after finalization.
pub fn prepare_wav_edit<W: Write + Seek>(
    request: &WavEditPreparationRequest,
    decoded_byte_budget: usize,
    sink: &mut W,
) -> Result<(), String> {
    prepare_wav_edit_with_cancel(request, decoded_byte_budget, sink, None)
}

/// Prepare edited WAV bytes with cooperative cancellation through decoding, effects and output.
///
/// Cancellation is checked at least every 4,096 decoded samples and by the reused transform
/// and encoder. The source read lease remains owned through the final revision check. The
/// sink must be fresh and empty; it must not alias the source or be externally published while
/// preparation runs. Failed or canceled output must be discarded, even if readable.
pub fn prepare_wav_edit_with_cancel<W: Write + Seek>(
    request: &WavEditPreparationRequest,
    decoded_byte_budget: usize,
    sink: &mut W,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    check_cancel(cancellation)?;
    // Public request fields are revalidated at execution, not trusted after construction.
    WavEditPreparationRequest::with_operation(
        request.context.clone(),
        request.range,
        request.operation,
    )?;
    let context = &request.context;
    let sample_count = checked_sample_count(context, decoded_byte_budget)?;
    let padded_crop = if let WavEditOperation::CropWithSilence {
        start_frame,
        end_frame,
    } = request.operation
    {
        Some(crate::padded_crop::PaddedCropPlan::new(
            context.total_frames,
            usize::from(context.spec.channels),
            start_frame,
            end_frame,
        )?)
    } else {
        None
    };
    let capacity = sample_count.max(padded_crop.as_ref().map_or(0, |plan| plan.output_samples));
    check_sample_budget(capacity, decoded_byte_budget)?;
    crate::edited_wav::checked_spec(
        padded_crop
            .as_ref()
            .map_or(sample_count, |plan| plan.output_samples),
        usize::from(context.spec.channels),
        context.spec.sample_rate,
    )?;
    let whole_source = WavSpanExportRequest::from_frames(context.clone(), 0, context.total_frames)?;
    let mut opened = detail::open_wav(&context.path, cancellation)?;
    if crate::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
        || opened.observation.strong_revision().as_ref() != Some(&context.file_revision)
    {
        return Err(String::from("stale WAV edit preparation request"));
    }
    crate::span_export::validate_source_and_rewind(&whole_source, &mut opened, cancellation)?;
    recheck(context, &opened, cancellation)?;
    check_cancel(cancellation)?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(capacity)
        .map_err(|error| format!("failed to reserve WAV edit buffer: {error}"))?;
    match opened.spec.sample_format {
        hound::SampleFormat::Float => {
            decode_samples(
                opened.reader.samples::<f32>(),
                sample_count,
                &mut samples,
                |value| value,
                cancellation,
            )?;
        }
        hound::SampleFormat::Int => {
            let scale = (1_i64 << (opened.spec.bits_per_sample - 1)) as f32;
            decode_samples(
                opened.reader.samples::<i32>(),
                sample_count,
                &mut samples,
                |value| value as f32 / scale,
                cancellation,
            )?;
        }
    }
    recheck(context, &opened, cancellation)?;
    match request.operation {
        WavEditOperation::GainAndFades(settings) => crate::apply_waveform_edit_effects_with_cancel(
            &mut samples,
            usize::from(context.spec.channels),
            context.spec.sample_rate,
            request.range.start_frame,
            request.range.end_frame,
            settings,
            cancellation,
        )?,
        WavEditOperation::Reverse | WavEditOperation::Mute => {
            let channels = usize::from(context.spec.channels);
            let selected = &mut samples
                [request.range.start_frame * channels..request.range.end_frame * channels];
            apply_frame_operation(selected, channels, request.operation, || {
                check_cancel(cancellation)
            })?;
        }
        WavEditOperation::Trim | WavEditOperation::Crop => {
            let channels = usize::from(context.spec.channels);
            retain_edit_frames(
                &mut samples,
                request.range.start_frame * channels,
                request.range.end_frame * channels,
                request.operation,
                || check_cancel(cancellation),
            )?;
        }
        WavEditOperation::CropWithSilence { .. } => {
            padded_crop
                .as_ref()
                .expect("validated padded crop plan")
                .apply(&mut samples, || check_cancel(cancellation))?;
        }
        WavEditOperation::Slide { frame_offset } => {
            slide_frames(
                &mut samples,
                usize::from(context.spec.channels),
                frame_offset,
                || check_cancel(cancellation),
            )?;
        }
    }
    recheck(context, &opened, cancellation)?;
    crate::encode_edited_wav_with_cancel(
        &samples,
        usize::from(context.spec.channels),
        context.spec.sample_rate,
        sink,
        cancellation,
    )?;
    recheck(context, &opened, cancellation)
}

/// Prepare whole-file peak normalization in an empty caller-owned sink.
///
/// This uses the edit preparation's bounded decode and revision fence. It does not publish or
/// replace the source. Silent and already-normalized inputs leave the sink empty; nonfinite
/// samples are rejected instead of being silently turned into audio. The caller must discard
/// the sink on any error, including a stale-source error after finalization.
pub fn prepare_wav_normalization<W: Write + Seek>(
    context: &WaveformDetailContext,
    decoded_byte_budget: usize,
    sink: &mut W,
) -> Result<WavNormalizationPreparationOutcome, String> {
    prepare_wav_normalization_with_cancel(context, decoded_byte_budget, sink, None)
}

/// Prepare whole-file peak normalization with cooperative decode and encoding cancellation.
pub fn prepare_wav_normalization_with_cancel<W: Write + Seek>(
    context: &WaveformDetailContext,
    decoded_byte_budget: usize,
    sink: &mut W,
    cancellation: Option<&AtomicBool>,
) -> Result<WavNormalizationPreparationOutcome, String> {
    check_cancel(cancellation)?;
    if sink
        .stream_position()
        .map_err(|error| format!("failed to inspect normalization sink: {error}"))?
        != 0
        || sink
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("failed to inspect normalization sink length: {error}"))?
            != 0
    {
        return Err(String::from("normalization sink must be fresh and empty"));
    }
    sink.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind normalization sink: {error}"))?;
    let sample_count = checked_sample_count(context, decoded_byte_budget)?;
    crate::edited_wav::checked_spec(
        sample_count,
        usize::from(context.spec.channels),
        context.spec.sample_rate,
    )?;
    let whole_source = WavSpanExportRequest::from_frames(context.clone(), 0, context.total_frames)?;
    let mut opened = detail::open_wav(&context.path, cancellation)?;
    if crate::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
        || opened.observation.strong_revision().as_ref() != Some(&context.file_revision)
    {
        return Err(String::from("stale WAV normalization request"));
    }
    crate::span_export::validate_source_and_rewind(&whole_source, &mut opened, cancellation)?;
    recheck(context, &opened, cancellation)?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
        .map_err(|error| format!("failed to reserve WAV normalization buffer: {error}"))?;
    match opened.spec.sample_format {
        hound::SampleFormat::Float => decode_samples(
            opened.reader.samples::<f32>(),
            sample_count,
            &mut samples,
            |value| value,
            cancellation,
        )?,
        hound::SampleFormat::Int => {
            let scale = (1_i64 << (opened.spec.bits_per_sample - 1)) as f32;
            decode_samples(
                opened.reader.samples::<i32>(),
                sample_count,
                &mut samples,
                |value| value as f32 / scale,
                cancellation,
            )?;
        }
    }
    recheck(context, &opened, cancellation)?;
    let mut peak = 0.0_f32;
    for block in samples.chunks(4_096) {
        check_cancel(cancellation)?;
        for &sample in block {
            if !sample.is_finite() {
                return Err(String::from("WAV normalization rejects nonfinite samples"));
            }
            peak = peak.max(sample.abs());
        }
    }
    if peak <= f32::EPSILON {
        return Ok(WavNormalizationPreparationOutcome::SkippedSilent);
    }
    if (1.0 - peak).abs() <= NORMALIZED_PEAK_TOLERANCE {
        return Ok(WavNormalizationPreparationOutcome::SkippedAlreadyNormalized);
    }
    check_cancel(cancellation)?;
    yield_analysis::normalize_peak_in_place(&mut samples);
    recheck(context, &opened, cancellation)?;
    crate::encode_edited_wav_with_cancel(
        &samples,
        usize::from(context.spec.channels),
        context.spec.sample_rate,
        sink,
        cancellation,
    )?;
    recheck(context, &opened, cancellation)?;
    Ok(WavNormalizationPreparationOutcome::Prepared)
}

fn checked_sample_count(context: &WaveformDetailContext, budget: usize) -> Result<usize, String> {
    let sample_count = context
        .total_frames
        .checked_mul(usize::from(context.spec.channels))
        .ok_or_else(|| String::from("WAV edit sample count overflows"))?;
    check_sample_budget(sample_count, budget)?;
    Ok(sample_count)
}

fn slide_frames(
    samples: &mut [f32],
    channels: usize,
    frame_offset: i64,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    let frames = samples.len() / channels;
    if frames <= 1 {
        return Ok(());
    }
    let total =
        i64::try_from(frames).map_err(|_| String::from("slide frame count is too large"))?;
    let right = frame_offset.rem_euclid(total) as usize;
    if right == 0 {
        return Ok(());
    }
    // Three frame reversals perform a cyclic rotation without a second sample buffer.
    apply_frame_operation(samples, channels, WavEditOperation::Reverse, &mut check)?;
    let (prefix, suffix) = samples.split_at_mut(right * channels);
    apply_frame_operation(prefix, channels, WavEditOperation::Reverse, &mut check)?;
    apply_frame_operation(suffix, channels, WavEditOperation::Reverse, &mut check)
}

fn check_sample_budget(sample_count: usize, budget: usize) -> Result<(), String> {
    let bytes = sample_count
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| String::from("WAV edit decoded size overflows"))?;
    if bytes > budget.min(MAX_WAV_EDIT_DECODED_BYTES) {
        return Err(String::from("WAV edit exceeds decoded memory budget"));
    }
    Ok(())
}

// Inputs are validated sample offsets. Forward copy order preserves unread source samples
// even when a chunk overlaps its destination. No second decoded buffer is allocated.
fn retain_edit_frames(
    samples: &mut Vec<f32>,
    start: usize,
    end: usize,
    operation: WavEditOperation,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    let (source_start, destination_start, count, output_len) = match operation {
        WavEditOperation::Trim => (
            end,
            start,
            samples.len() - end,
            samples.len() - (end - start),
        ),
        WavEditOperation::Crop => (start, 0, end - start, end - start),
        _ => unreachable!("only trim and crop change frame count"),
    };
    if source_start != destination_start {
        for offset in (0..count).step_by(4096) {
            check()?;
            let chunk_len = (count - offset).min(4096);
            samples.copy_within(
                source_start + offset..source_start + offset + chunk_len,
                destination_start + offset,
            );
        }
    }
    check()?;
    samples.truncate(output_len);
    Ok(())
}

// Called only with a validated complete-frame slice. Cancellation leaves a partial buffer
// that the preparation caller discards before encoding.
fn apply_frame_operation(
    selected: &mut [f32],
    channels: usize,
    operation: WavEditOperation,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    match operation {
        WavEditOperation::Reverse => {
            let frames = selected.len() / channels;
            let mut changed = 0;
            for left in 0..frames / 2 {
                let right = frames - 1 - left;
                for channel in 0..channels {
                    if changed == 4096 {
                        check()?;
                        changed = 0;
                    }
                    selected.swap(left * channels + channel, right * channels + channel);
                    changed += 2;
                }
            }
        }
        WavEditOperation::Mute => {
            for block in selected.chunks_mut(4096) {
                check()?;
                block.fill(0.0);
            }
        }
        _ => unreachable!("only reverse and mute use this transform"),
    }
    check()
}

fn check_cancel(cancellation: Option<&AtomicBool>) -> Result<(), String> {
    if crate::is_cancelled(cancellation) {
        Err(String::from("WAV edit preparation canceled"))
    } else {
        Ok(())
    }
}

fn recheck(
    context: &WaveformDetailContext,
    opened: &detail::OpenWav,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    check_cancel(cancellation)?;
    let current = detail::recheck_wav(&context.path, opened, cancellation)
        .map_err(|error| format!("WAV edit source changed or unavailable: {error}"))?;
    detail::ensure_file_unchanged(&opened.observation, &current)?;
    if current.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV edit preparation source"));
    }
    check_cancel(cancellation)
}

fn decode_samples<T>(
    mut source: impl Iterator<Item = Result<T, hound::Error>>,
    sample_count: usize,
    output: &mut Vec<f32>,
    convert: impl Fn(T) -> f32,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    for index in 0..sample_count {
        if index % 4_096 == 0 {
            check_cancel(cancellation)?;
        }
        let value = source
            .next()
            .ok_or_else(|| String::from("incomplete WAV edit source"))?
            .map_err(|error| format!("failed to decode WAV edit source: {error}"))?;
        output.push(convert(value));
    }
    check_cancel(cancellation)?;
    if source.next().is_some() {
        return Err(String::from("WAV edit source has extra samples"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{self, Cursor, SeekFrom},
        path::Path,
        sync::atomic::Ordering,
    };
    use tempfile::TempDir;
    use yield_core::WaveformEditFade;

    fn write_int(path: &Path, samples: &[i32], channels: u16, bits: u16) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels,
                sample_rate: 1_000,
                bits_per_sample: bits,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn request(path: &Path) -> WavEditPreparationRequest {
        let source = WavSpanExportRequest::from_path_span(path, 0.0, 1.0).unwrap();
        WavEditPreparationRequest::new(
            source.context,
            source.range,
            WaveformEditSettings::new(1.0, None, None).unwrap(),
        )
        .unwrap()
    }

    fn read_output(output: Cursor<Vec<u8>>) -> (hound::WavSpec, Vec<f32>) {
        let mut reader = hound::WavReader::new(Cursor::new(output.into_inner())).unwrap();
        (
            reader.spec(),
            reader.samples::<f32>().collect::<Result<_, _>>().unwrap(),
        )
    }

    fn write_float(path: &Path, samples: &[f32], channels: usize) {
        let mut encoded = Cursor::new(Vec::new());
        crate::encode_edited_wav(samples, channels, 48_000, &mut encoded).unwrap();
        fs::write(path, encoded.into_inner()).unwrap();
    }

    #[test]
    fn normalization_skips_silence_and_reference_peak_tolerance_without_writing() {
        let dir = TempDir::new().unwrap();
        for (samples, expected) in [
            (
                vec![0.0, -0.0, 0.0],
                WavNormalizationPreparationOutcome::SkippedSilent,
            ),
            (
                vec![0.5, -1.0, 0.25],
                WavNormalizationPreparationOutcome::SkippedAlreadyNormalized,
            ),
            (
                vec![0.5, -0.9995, 0.25],
                WavNormalizationPreparationOutcome::SkippedAlreadyNormalized,
            ),
        ] {
            let path = dir.path().join("normalization-skip.wav");
            write_float(&path, &samples, 1);
            let source_bytes = fs::read(&path).unwrap();
            let mut output = Cursor::new(Vec::new());
            assert_eq!(
                prepare_wav_normalization(&request(&path).context, 12, &mut output).unwrap(),
                expected
            );
            assert!(output.into_inner().is_empty());
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
    }

    #[test]
    fn normalization_uses_one_peak_across_interleaved_channels() {
        let dir = TempDir::new().unwrap();
        for (channels, samples, expected) in [
            (1, vec![0.25, -0.5], vec![0.5, -1.0]),
            (
                2,
                vec![0.25, -0.5, 0.125, -0.25],
                vec![0.5, -1.0, 0.25, -0.5],
            ),
            (3, vec![0.25, -2.0, 0.5], vec![0.125, -1.0, 0.25]),
        ] {
            let path = dir.path().join(format!("normalization-{channels}.wav"));
            write_float(&path, &samples, channels);
            let source_bytes = fs::read(&path).unwrap();
            let mut output = Cursor::new(Vec::new());
            assert_eq!(
                prepare_wav_normalization(&request(&path).context, samples.len() * 4, &mut output,)
                    .unwrap(),
                WavNormalizationPreparationOutcome::Prepared
            );
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, channels as u16);
            assert_eq!(spec.sample_rate, 48_000);
            assert_eq!(actual, expected);
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
    }

    #[test]
    fn normalization_rejects_nonfinite_and_budget_without_touching_source() {
        let dir = TempDir::new().unwrap();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let path = dir.path().join("normalization-bad.wav");
            write_float(&path, &[0.25, bad], 1);
            let source_bytes = fs::read(&path).unwrap();
            let mut output = Cursor::new(Vec::new());
            let error = prepare_wav_normalization(&request(&path).context, 8, &mut output)
                .expect_err("nonfinite input must be rejected");
            assert!(error.contains("nonfinite"), "{error}");
            assert!(output.into_inner().is_empty());
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
        let path = dir.path().join("normalization-budget.wav");
        write_int(&path, &[100, -100], 1, 16);
        let source_bytes = fs::read(&path).unwrap();
        let mut output = Cursor::new(Vec::new());
        assert!(prepare_wav_normalization(&request(&path).context, 4, &mut output).is_err());
        assert!(output.into_inner().is_empty());
        assert_eq!(fs::read(&path).unwrap(), source_bytes);
    }

    #[test]
    fn slide_wraps_stereo_frames_for_signed_and_extreme_offsets() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slide.wav");
        let original = [1, 11, 2, 12, 3, 13, 4, 14];
        write_int(&path, &original, 2, 16);
        let source_bytes = fs::read(&path).unwrap();
        for (offset, expected) in [
            (1, [4, 14, 1, 11, 2, 12, 3, 13]),
            (-1, [2, 12, 3, 13, 4, 14, 1, 11]),
            (0, original),
            (4, original),
            (-4, original),
            (i64::MIN, original),
            (i64::MAX, [2, 12, 3, 13, 4, 14, 1, 11]),
        ] {
            let request =
                WavEditPreparationRequest::with_slide(request(&path).context, offset).unwrap();
            let mut output = Cursor::new(Vec::new());
            prepare_wav_edit(&request, 32, &mut output).unwrap();
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, 2);
            assert_eq!(actual, expected.map(|value| value as f32 / 32768.0));
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
        let mut request = WavEditPreparationRequest::with_slide(request(&path).context, 1).unwrap();
        request.range.start_frame = 1;
        let mut output = Cursor::new(Vec::new());
        assert!(prepare_wav_edit(&request, 32, &mut output).is_err());
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn slide_checks_cancellation_through_each_reversal_without_allocating() {
        let original: Vec<f32> = (0..30_000).map(|value| value as f32).collect();
        let mut samples = original.clone();
        let pointer = samples.as_ptr();
        let mut checks = 0;
        slide_frames(&mut samples, 3, 1302, || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        let mut expected = original.clone();
        expected.rotate_right(1302 * 3);
        assert_eq!(samples, expected);
        assert_eq!(samples.as_ptr(), pointer);
        for stop_at in 1..=checks {
            let mut samples = original.clone();
            let mut visited = 0;
            assert!(
                slide_frames(&mut samples, 3, 1302, || {
                    visited += 1;
                    if visited == stop_at {
                        Err(String::from("canceled fixture"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(visited, stop_at);
        }
        let mut single = [-0.0_f32, 1.0];
        slide_frames(&mut single, 2, i64::MIN, || Ok(())).unwrap();
        assert_eq!(single.map(f32::to_bits), [-0.0_f32, 1.0].map(f32::to_bits));
    }

    #[test]
    fn waveform_regions_drive_crop_padding_and_existing_audio_edits() {
        use yield_core::WaveformEditRegion;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("region.wav");
        write_int(&path, &[1, 2, 3, 4, 5, 6, 7, 8], 1, 16);
        let context = request(&path).context;
        let crop = WavEditPreparationRequest::from_edit_region(
            context.clone(),
            WaveformEditRegion::new(-0.25, 0.375).unwrap(),
            WavEditOperation::Crop,
        )
        .unwrap();
        assert_eq!(
            crop.operation,
            WavEditOperation::CropWithSilence {
                start_frame: -2,
                end_frame: 3
            }
        );
        let mut output = Cursor::new(Vec::new());
        prepare_wav_edit(&crop, 32, &mut output).unwrap();
        assert_eq!(
            read_output(output).1,
            [0, 0, 1, 2, 3].map(|value| value as f32 / 32768.0)
        );

        let mute = WavEditPreparationRequest::from_edit_region(
            context,
            WaveformEditRegion::new(-0.25, 0.26).unwrap(),
            WavEditOperation::Mute,
        )
        .unwrap();
        assert_eq!(
            mute.range,
            WaveformDetailRange {
                start_frame: 0,
                end_frame: 3
            }
        );
        let mut output = Cursor::new(Vec::new());
        prepare_wav_edit(&mute, 32, &mut output).unwrap();
        assert_eq!(
            read_output(output).1,
            [0, 0, 0, 4, 5, 6, 7, 8].map(|value| value as f32 / 32768.0)
        );
    }

    #[test]
    fn waveform_region_admission_rejects_absent_audio_and_conflicting_crop_bounds() {
        use yield_core::WaveformEditRegion;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("outside.wav");
        write_int(&path, &[1, 2, 3, 4], 1, 16);
        let context = request(&path).context;
        for operation in [
            WavEditOperation::Mute,
            WavEditOperation::Reverse,
            WavEditOperation::Trim,
            WavEditOperation::GainAndFades(WaveformEditSettings::new(1.0, None, None).unwrap()),
        ] {
            assert!(
                WavEditPreparationRequest::from_edit_region(
                    context.clone(),
                    WaveformEditRegion::new(1.0, 2.0).unwrap(),
                    operation
                )
                .is_err()
            );
        }
        assert!(
            WavEditPreparationRequest::from_edit_region(
                context.clone(),
                WaveformEditRegion::new(1.0, 2.0).unwrap(),
                WavEditOperation::Crop
            )
            .is_ok()
        );
        assert!(
            WavEditPreparationRequest::from_edit_region(
                context,
                WaveformEditRegion::new(0.0, 1.0).unwrap(),
                WavEditOperation::CropWithSilence {
                    start_frame: 0,
                    end_frame: 1
                }
            )
            .is_err()
        );
    }

    #[test]
    fn padded_crop_prepares_exact_stereo_silence_and_source_intersection() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("padded.wav");
        write_int(&path, &[1, 11, 2, 12, 3, 13, 4, 14], 2, 16);
        let source_bytes = fs::read(&path).unwrap();
        for (start, end, expected) in [
            (-2, 2, vec![0, 0, 0, 0, 1, 11, 2, 12]),
            (2, 6, vec![3, 13, 4, 14, 0, 0, 0, 0]),
            (-1, 5, vec![0, 0, 1, 11, 2, 12, 3, 13, 4, 14, 0, 0]),
            (-4, -2, vec![0, 0, 0, 0]),
            (6, 8, vec![0, 0, 0, 0]),
            (1, 3, vec![2, 12, 3, 13]),
        ] {
            let request =
                WavEditPreparationRequest::with_padded_crop(request(&path).context, start, end)
                    .unwrap();
            let mut output = Cursor::new(Vec::new());
            prepare_wav_edit(&request, 64, &mut output).unwrap();
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, 2);
            assert_eq!(spec.sample_rate, 1_000);
            assert_eq!(
                actual,
                expected
                    .iter()
                    .map(|value| *value as f32 / 32768.0)
                    .collect::<Vec<_>>()
            );
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
    }

    #[test]
    fn padded_crop_rejects_invalid_bounds_and_output_budget_before_writing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("budget.wav");
        write_int(&path, &[1, 2, 3, 4, 5, 6, 7, 8], 2, 16);
        let context = request(&path).context;
        for (start, end) in [(0, 0), (2, 1), (i64::MIN, i64::MAX)] {
            assert!(
                WavEditPreparationRequest::with_padded_crop(context.clone(), start, end).is_err()
            );
        }
        let mut request = WavEditPreparationRequest::with_padded_crop(context, -2, 6).unwrap();
        let mut output = Cursor::new(Vec::new());
        assert!(
            prepare_wav_edit(&request, 32, &mut output)
                .unwrap_err()
                .contains("budget")
        );
        assert!(output.get_ref().is_empty());
        request.operation = WavEditOperation::CropWithSilence {
            start_frame: 0,
            end_frame: i64::MAX,
        };
        assert!(prepare_wav_edit(&request, usize::MAX, &mut output).is_err());
        assert!(output.get_ref().is_empty());
        request.operation = WavEditOperation::CropWithSilence {
            start_frame: -1,
            end_frame: 3,
        };
        request.range.start_frame = 1;
        assert!(prepare_wav_edit(&request, 64, &mut output).is_err());
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn trim_and_crop_preserve_stereo_frames_and_source() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("retain.wav");
        let original = [1, 11, 2, 12, 3, 13, 4, 14, 5, 15, 6, 16];
        write_int(&path, &original, 2, 16);
        let source_bytes = fs::read(&path).unwrap();
        for (operation, start, end, expected) in [
            (
                WavEditOperation::Trim,
                1,
                3,
                vec![1, 11, 4, 14, 5, 15, 6, 16],
            ),
            (
                WavEditOperation::Trim,
                0,
                2,
                vec![3, 13, 4, 14, 5, 15, 6, 16],
            ),
            (
                WavEditOperation::Trim,
                4,
                6,
                vec![1, 11, 2, 12, 3, 13, 4, 14],
            ),
            (WavEditOperation::Crop, 1, 4, vec![2, 12, 3, 13, 4, 14]),
            (WavEditOperation::Crop, 0, 6, original.to_vec()),
        ] {
            let request = WavEditPreparationRequest::with_operation(
                request(&path).context,
                WaveformDetailRange {
                    start_frame: start,
                    end_frame: end,
                },
                operation,
            )
            .unwrap();
            let mut output = Cursor::new(Vec::new());
            prepare_wav_edit(&request, 48, &mut output).unwrap();
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, 2);
            assert_eq!(spec.sample_rate, 1_000);
            assert_eq!(
                actual,
                expected
                    .iter()
                    .map(|value| *value as f32 / 32768.0)
                    .collect::<Vec<_>>()
            );
            assert_eq!(fs::read(&path).unwrap(), source_bytes);
        }
    }

    #[test]
    fn whole_file_trim_is_rejected_at_construction_and_execution() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("whole.wav");
        write_int(&path, &[1, 2, 3, 4], 2, 16);
        let mut request = request(&path);
        assert!(
            WavEditPreparationRequest::with_operation(
                request.context.clone(),
                request.range,
                WavEditOperation::Trim
            )
            .is_err()
        );
        request.operation = WavEditOperation::Trim;
        let mut output = Cursor::new(Vec::new());
        let error = prepare_wav_edit(&request, 16, &mut output).unwrap_err();
        assert!(error.contains("no audio"));
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn retention_handles_overlapping_chunks_without_allocating_another_buffer() {
        let original: Vec<f32> = (0..12_000).map(|value| value as f32).collect();
        for operation in [WavEditOperation::Trim, WavEditOperation::Crop] {
            let mut samples = original.clone();
            let pointer = samples.as_ptr();
            let capacity = samples.capacity();
            let (start, end) = if operation == WavEditOperation::Trim {
                (2, 4)
            } else {
                (2, 11_998)
            };
            let expected: Vec<_> = if operation == WavEditOperation::Trim {
                original[..start]
                    .iter()
                    .chain(&original[end..])
                    .copied()
                    .collect()
            } else {
                original[start..end].to_vec()
            };
            retain_edit_frames(&mut samples, start, end, operation, || Ok(())).unwrap();
            assert_eq!(samples, expected);
            assert_eq!(samples.as_ptr(), pointer);
            assert_eq!(samples.capacity(), capacity);

            let mut samples = original.clone();
            let mut checks = 0;
            let result = retain_edit_frames(&mut samples, start, end, operation, || {
                checks += 1;
                if checks == 3 {
                    Err(String::from("canceled fixture"))
                } else {
                    Ok(())
                }
            });
            assert!(result.unwrap_err().contains("canceled"));
            assert_eq!(samples.len(), original.len());
            assert_eq!(
                samples
                    .iter()
                    .zip(&original)
                    .filter(|(left, right)| left != right)
                    .count(),
                4096
            );
        }
    }

    #[test]
    fn reverse_preserves_stereo_pairs_and_unselected_frames() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("reverse.wav");
        write_int(&path, &[1, 11, 2, 12, 3, 13, 4, 14, 5, 15, 6, 16], 2, 16);
        for (start, end, expected) in [
            (1, 4, [1, 11, 4, 14, 3, 13, 2, 12, 5, 15, 6, 16]),
            (1, 5, [1, 11, 5, 15, 4, 14, 3, 13, 2, 12, 6, 16]),
            (2, 3, [1, 11, 2, 12, 3, 13, 4, 14, 5, 15, 6, 16]),
        ] {
            let request = WavEditPreparationRequest::with_operation(
                request(&path).context,
                WaveformDetailRange {
                    start_frame: start,
                    end_frame: end,
                },
                WavEditOperation::Reverse,
            )
            .unwrap();
            let mut output = Cursor::new(Vec::new());
            prepare_wav_edit(&request, 48, &mut output).unwrap();
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, 2);
            assert_eq!(actual, expected.map(|value| value as f32 / 32768.0));
        }
    }

    #[test]
    fn mute_changes_only_selected_frames_to_positive_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mute.wav");
        let original = [-0.0_f32, 2.0, -1.0, 0.5, 3.0, -0.25, 0.75, -2.0];
        let mut source = Cursor::new(Vec::new());
        crate::encode_edited_wav(&original, 2, 48_000, &mut source).unwrap();
        fs::write(&path, source.into_inner()).unwrap();
        let mut request = request(&path);
        request.operation = WavEditOperation::Mute;
        request.range = WaveformDetailRange {
            start_frame: 1,
            end_frame: 3,
        };
        let mut output = Cursor::new(Vec::new());
        prepare_wav_edit(&request, 32, &mut output).unwrap();
        let (_, actual) = read_output(output);
        let expected = [-0.0_f32, 2.0, 0.0, 0.0, 0.0, 0.0, 0.75, -2.0];
        assert_eq!(
            actual.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            expected.map(f32::to_bits)
        );
        assert_eq!(fs::read(&path).unwrap(), {
            let mut unchanged = Cursor::new(Vec::new());
            crate::encode_edited_wav(&original, 2, 48_000, &mut unchanged).unwrap();
            unchanged.into_inner()
        });
    }

    #[test]
    fn frame_operations_check_cancellation_inside_large_frames() {
        for operation in [WavEditOperation::Reverse, WavEditOperation::Mute] {
            let mut samples: Vec<f32> = (1..=20_000).map(|value| value as f32).collect();
            let original = samples.clone();
            let mut checks = 0;
            let stop_at = if operation == WavEditOperation::Mute {
                3
            } else {
                2
            };
            let result = apply_frame_operation(&mut samples, 10_000, operation, || {
                checks += 1;
                if checks == stop_at {
                    Err(String::from("canceled fixture"))
                } else {
                    Ok(())
                }
            });
            assert!(result.unwrap_err().contains("canceled"));
            assert_eq!(
                samples
                    .iter()
                    .zip(original)
                    .filter(|(left, right)| **left != *right)
                    .count(),
                4096
            );
        }
    }

    #[test]
    fn integer_formats_use_native_normalization_without_downmixing() {
        let dir = TempDir::new().unwrap();
        for bits in [8, 16, 24, 32] {
            let path = dir.path().join(format!("{bits}.wav"));
            let scale = 1_i64 << (bits - 1);
            let samples = [
                (-scale) as i32,
                (scale - 1) as i32,
                0,
                1,
                -1,
                (scale / 2) as i32,
            ];
            write_int(&path, &samples, 2, bits);
            let mut output = Cursor::new(Vec::new());
            prepare_wav_edit(&request(&path), 24, &mut output).unwrap();
            let (spec, actual) = read_output(output);
            assert_eq!(spec.channels, 2);
            assert_eq!(spec.sample_rate, 1_000);
            assert_eq!(spec.sample_format, hound::SampleFormat::Float);
            assert_eq!(spec.bits_per_sample, 32);
            assert_eq!(actual, samples.map(|sample| sample as f32 / scale as f32));
        }
    }

    #[test]
    fn float_input_preserves_unclipped_values_and_signed_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("float.wav");
        let original = [2.5_f32, -0.0, -3.0, 0.5];
        let mut source = Cursor::new(Vec::new());
        crate::encode_edited_wav(&original, 2, 48_000, &mut source).unwrap();
        fs::write(&path, source.into_inner()).unwrap();
        let mut output = Cursor::new(Vec::new());
        prepare_wav_edit(&request(&path), 16, &mut output).unwrap();
        let (_, actual) = read_output(output);
        assert_eq!(
            actual.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            original.map(f32::to_bits)
        );
    }

    #[test]
    fn edits_selection_and_outer_fades_while_retaining_whole_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("effects.wav");
        write_int(&path, &[24_576; 64], 2, 16);
        let mut request = request(&path);
        request.range = WaveformDetailRange {
            start_frame: 8,
            end_frame: 24,
        };
        request.operation = WavEditOperation::GainAndFades(
            WaveformEditSettings::new(
                2.0,
                Some(WaveformEditFade::new(0.0, 0.0, 0.25, 0.5).unwrap()),
                Some(WaveformEditFade::new(0.0, 0.0, 0.25, 0.5).unwrap()),
            )
            .unwrap(),
        );
        let mut output = Cursor::new(Vec::new());
        prepare_wav_edit(&request, 256, &mut output).unwrap();
        let (_, actual) = read_output(output);
        assert_eq!(actual.len(), 64);
        assert_eq!(&actual[..8], &[0.75; 8]);
        assert!(actual[8..16].iter().any(|s| *s < 0.75));
        assert_eq!(&actual[24..40], &[1.5; 16]);
        assert!(actual[48..56].iter().any(|s| *s < 0.75));
        assert_eq!(&actual[56..], &[0.75; 8]);
    }

    #[test]
    fn rejects_stale_or_invalid_requests_and_budget_before_output() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("source.wav");
        write_int(&path, &[1, 2, 3, 4], 2, 16);
        let request = request(&path);
        let mut output = Cursor::new(Vec::new());
        assert!(
            prepare_wav_edit(&request, 15, &mut output)
                .unwrap_err()
                .contains("budget")
        );
        assert!(output.get_ref().is_empty());
        let mut invalid = request.clone();
        invalid.range.end_frame = 3;
        assert!(prepare_wav_edit(&invalid, 16, &mut output).is_err());
        let mut huge = request.context.clone();
        huge.total_frames = usize::MAX;
        assert!(checked_sample_count(&huge, usize::MAX).is_err());
        huge.total_frames = MAX_WAV_EDIT_DECODED_BYTES;
        assert!(checked_sample_count(&huge, usize::MAX).is_err());
        let old = dir.path().join("old.wav");
        fs::rename(&path, old).unwrap();
        write_int(&path, &[5, 6, 7, 8], 2, 16);
        assert!(
            prepare_wav_edit(&request, 16, &mut output)
                .unwrap_err()
                .contains("stale")
        );
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn malformed_riff_bounds_are_rejected_without_output() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("malformed.wav");
        write_int(&path, &[1, 2, 3], 1, 16);
        let mut bytes = fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let request = request(&path);
        let mut output = Cursor::new(Vec::new());
        assert!(
            prepare_wav_edit(&request, 12, &mut output)
                .unwrap_err()
                .contains("RIFF")
        );
        assert!(output.get_ref().is_empty());
    }

    struct ActionSink<F> {
        inner: Cursor<Vec<u8>>,
        action: Option<F>,
    }
    impl<F: FnOnce() -> io::Result<()>> Write for ActionSink<F> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(action) = self.action.take() {
                action()?;
            }
            self.inner.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<F> Seek for ActionSink<F> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn replacement_during_output_rejects_the_finalized_payload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("source.wav");
        let replacement = dir.path().join("replacement.wav");
        let old = dir.path().join("old.wav");
        write_int(&path, &[1, 2, 3], 1, 16);
        write_int(&replacement, &[4, 5, 6], 1, 16);
        let request = request(&path);
        let mut output = ActionSink {
            inner: Cursor::new(Vec::new()),
            action: Some(|| {
                fs::rename(&path, &old)?;
                fs::rename(&replacement, &path)
            }),
        };
        let error = prepare_wav_edit(&request, 12, &mut output).unwrap_err();
        assert!(
            error.contains("changed") || error.contains("stale"),
            "{error}"
        );
        assert!(!output.inner.get_ref().is_empty());
        assert_eq!(read_output(output.inner).1.len(), 3);
        // The source lease is released after the failed preparation on Windows too.
        fs::OpenOptions::new().write(true).open(&old).unwrap();
    }

    #[test]
    fn cancellation_is_checked_before_work_during_decode_and_output() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("source.wav");
        write_int(&path, &[1, 2, 3], 1, 16);
        let request = request(&path);
        let cancel = AtomicBool::new(true);
        let mut output = Cursor::new(Vec::new());
        assert!(
            prepare_wav_edit_with_cancel(&request, 12, &mut output, Some(&cancel))
                .unwrap_err()
                .contains("canceled")
        );
        assert!(output.get_ref().is_empty());
        cancel.store(false, Ordering::Relaxed);
        let mut decoded = Vec::with_capacity(10_000);
        let source = (0..10_000).map(|index| {
            if index == 7 {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(0.0_f32)
        });
        assert!(
            decode_samples(source, 10_000, &mut decoded, |value| value, Some(&cancel))
                .unwrap_err()
                .contains("canceled")
        );
        assert_eq!(decoded.len(), 4096);
        cancel.store(false, Ordering::Relaxed);
        let mut output = ActionSink {
            inner: Cursor::new(Vec::new()),
            action: Some(|| {
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            }),
        };
        assert!(
            prepare_wav_edit_with_cancel(&request, 12, &mut output, Some(&cancel))
                .unwrap_err()
                .contains("canceled")
        );
    }
}
