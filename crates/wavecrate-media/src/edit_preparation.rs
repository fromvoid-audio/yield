//! Revision-checked preparation of whole-file gain/fade edits, without publication.

use std::io::{Seek, Write};
use std::sync::atomic::AtomicBool;

use wavecrate_core::WaveformEditSettings;

use crate::{WavSpanExportRequest, WaveformDetailContext, WaveformDetailRange, detail};

/// Hard ceiling for the interleaved decoded buffer used by edit preparation (256 MiB).
pub const MAX_WAV_EDIT_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// A revision-fenced gain/fade edit of a half-open selection within a whole WAV file.
#[derive(Clone, Debug, PartialEq)]
pub struct WavEditPreparationRequest {
    /// Source identity, revision and format captured before background admission.
    pub context: WaveformDetailContext,
    /// Existing source frames to edit; outer fades may also change adjacent frames.
    pub range: WaveformDetailRange,
    /// Validated native gain and fade parameters.
    pub settings: WaveformEditSettings,
}

impl WavEditPreparationRequest {
    /// Validate the format and exact selection without decoding or writing audio.
    pub fn new(
        context: WaveformDetailContext,
        range: WaveformDetailRange,
        settings: WaveformEditSettings,
    ) -> Result<Self, String> {
        WavSpanExportRequest::new(context.clone(), range)?;
        Ok(Self {
            context,
            range,
            settings,
        })
    }
}

/// Prepare the whole edited WAV in a fresh caller-owned sink, without publishing it.
///
/// `decoded_byte_budget` bounds the decoded f32 buffer, additionally capped by
/// [`MAX_WAV_EDIT_DECODED_BYTES`]. The caller must run this blocking work off the UI thread.
/// Output retains all frames/channels and uses native unclipped 32-bit float encoding.
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
    WavEditPreparationRequest::new(request.context.clone(), request.range, request.settings)?;
    let context = &request.context;
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
        return Err(String::from("stale WAV edit preparation request"));
    }
    crate::span_export::validate_source_and_rewind(&whole_source, &mut opened, cancellation)?;
    recheck(context, &opened, cancellation)?;
    check_cancel(cancellation)?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
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
    crate::apply_waveform_edit_effects_with_cancel(
        &mut samples,
        usize::from(context.spec.channels),
        context.spec.sample_rate,
        request.range.start_frame,
        request.range.end_frame,
        request.settings,
        cancellation,
    )?;
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

fn checked_sample_count(context: &WaveformDetailContext, budget: usize) -> Result<usize, String> {
    let sample_count = context
        .total_frames
        .checked_mul(usize::from(context.spec.channels))
        .ok_or_else(|| String::from("WAV edit sample count overflows"))?;
    let bytes = sample_count
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| String::from("WAV edit decoded size overflows"))?;
    if bytes > budget.min(MAX_WAV_EDIT_DECODED_BYTES) {
        return Err(String::from("WAV edit exceeds decoded memory budget"));
    }
    Ok(sample_count)
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
    use wavecrate_core::WaveformEditFade;

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
        request.settings = WaveformEditSettings::new(
            2.0,
            Some(WaveformEditFade::new(0.0, 0.0, 0.25, 0.5).unwrap()),
            Some(WaveformEditFade::new(0.0, 0.0, 0.25, 0.5).unwrap()),
        )
        .unwrap();
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
