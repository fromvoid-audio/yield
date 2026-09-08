//! Exact, bounded-memory WAV peak preparation for one frame span.

use std::{fs::File, io::BufReader, sync::atomic::AtomicBool};

use crate::{WaveformDetailContext, WaveformDetailRange, detail};

const PEAK_READ_BLOCK_SAMPLES: usize = 4_096;

/// A file-fenced request for the raw peak of one half-open WAV frame span.
///
/// Unlike [`crate::WaveformDetailRequest`], this request has no visible-frame limit because the
/// result is one scalar and the decoder retains only its current sample and peak accumulator.
#[derive(Clone, Debug, PartialEq)]
pub struct WavSpanPeakRequest {
    /// Context captured by the source overview read.
    pub context: WaveformDetailContext,
    /// Exact half-open source frame range to inspect.
    pub range: WaveformDetailRange,
}

impl WavSpanPeakRequest {
    /// Validate and create a request for `range` in `context`.
    pub fn new(context: WaveformDetailContext, range: WaveformDetailRange) -> Result<Self, String> {
        let request = Self { context, range };
        request.validate()?;
        Ok(request)
    }

    /// Validate and create a request for `start_frame..end_frame` in `context`.
    pub fn from_frames(
        context: WaveformDetailContext,
        start_frame: usize,
        end_frame: usize,
    ) -> Result<Self, String> {
        Self::new(
            context,
            WaveformDetailRange {
                start_frame,
                end_frame,
            },
        )
    }

    fn validate(&self) -> Result<(), String> {
        if !self.context.is_valid() {
            return Err(String::from("WAV span peak context is unavailable"));
        }
        if self.range.start_frame >= self.range.end_frame {
            return Err(String::from("WAV span peak range must be non-empty"));
        }
        if self.range.end_frame > self.context.total_frames {
            return Err(String::from("WAV span peak range exceeds source frames"));
        }
        Ok(())
    }
}

/// Read the raw maximum absolute interleaved channel sample for one exact WAV span.
pub fn load_wav_span_peak(request: &WavSpanPeakRequest) -> Result<f32, String> {
    load_wav_span_peak_with_cancel(request, None)
}

/// Read one exact WAV span's raw maximum absolute interleaved channel sample with cancellation.
pub fn load_wav_span_peak_with_cancel(
    request: &WavSpanPeakRequest,
    cancellation: Option<&AtomicBool>,
) -> Result<f32, String> {
    request.validate()?;
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV span peak canceled"));
    }

    let context = &request.context;
    let mut opened = detail::open_wav(&context.path, cancellation).map_err(|error| {
        format!(
            "WAV span peak failed for {}: {error}",
            context.path.display()
        )
    })?;
    if detail::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
    {
        return Err(String::from("stale WAV span peak request"));
    }
    if opened.observation.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV span peak request"));
    }
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV span peak canceled"));
    }

    opened
        .reader
        .seek(
            u32::try_from(request.range.start_frame).map_err(|_| {
                String::from("WAV span peak range cannot be represented by WAV reader")
            })?,
        )
        .map_err(|error| format!("failed to seek WAV span peak source: {error}"))?;
    let peak = read_interleaved_peak(
        &mut opened.reader,
        opened.spec,
        request.range.len(),
        cancellation,
    )?;
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV span peak canceled"));
    }

    let after = detail::recheck_wav(
        &context.path,
        &opened.file,
        opened.spec,
        opened.total_frames,
        cancellation,
    )
    .map_err(|error| {
        format!(
            "WAV span peak failed for {}: {error}",
            context.path.display()
        )
    })?;
    detail::ensure_file_unchanged(&opened.observation, &after)
        .map_err(|_| String::from("stale WAV span peak result"))?;
    if after.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV span peak result"));
    }
    Ok(peak)
}

fn read_interleaved_peak(
    reader: &mut hound::WavReader<BufReader<File>>,
    spec: hound::WavSpec,
    frame_count: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<f32, String> {
    let channels = usize::from(spec.channels);
    match spec.sample_format {
        hound::SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            stream_sample_peak(
                &mut samples,
                channels,
                frame_count,
                cancellation,
                "float",
                |sample| sample,
            )
        }
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i16>();
            stream_sample_peak(
                &mut samples,
                channels,
                frame_count,
                cancellation,
                "integer",
                move |sample| f32::from(sample) / scale,
            )
        }
        hound::SampleFormat::Int => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i32>();
            stream_sample_peak(
                &mut samples,
                channels,
                frame_count,
                cancellation,
                "integer",
                move |sample| sample as f32 / scale,
            )
        }
    }
}

fn stream_sample_peak<T, I, D>(
    samples: &mut I,
    channels: usize,
    frame_count: usize,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
    mut decode: D,
) -> Result<f32, String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
    D: FnMut(T) -> f32,
{
    let mut peak = 0.0_f32;
    let mut samples_read = 0usize;
    for _ in 0..frame_count {
        if crate::is_cancelled(cancellation) {
            return Err(String::from("WAV span peak canceled"));
        }
        for _ in 0..channels {
            let sample = samples
                .next()
                .transpose()
                .map_err(|error| format!("failed to read {sample_kind} WAV sample: {error}"))?
                .ok_or_else(|| String::from("WAV ended before declared frame count"))?;
            peak = peak.max(decode(sample).abs());
            samples_read = samples_read.saturating_add(1);
            if samples_read.is_multiple_of(PEAK_READ_BLOCK_SAMPLES)
                && crate::is_cancelled(cancellation)
            {
                return Err(String::from("WAV span peak canceled"));
            }
        }
    }
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV span peak canceled"));
    }
    Ok(peak)
}

fn integer_sample_scale(bits_per_sample: u16) -> f32 {
    (1_i64 << bits_per_sample.saturating_sub(1).min(31)).max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::{
        fs::{self, OpenOptions},
        io::{Seek, SeekFrom, Write},
        path::Path,
        sync::atomic::Ordering,
    };
    use tempfile::TempDir;

    fn float_spec(channels: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        }
    }

    fn write_float_wav(path: &Path, channels: u16, frames: &[[f32; 2]]) {
        let spec = float_spec(channels);
        let mut writer = WavWriter::create(path, spec).expect("create WAV fixture");
        for frame in frames {
            for sample in frame.iter().take(usize::from(channels)) {
                writer.write_sample(*sample).expect("write WAV sample");
            }
        }
        writer.finalize().expect("finish WAV fixture");
    }

    fn context_for(path: &Path) -> WaveformDetailContext {
        crate::load_waveform_preview(path)
            .expect("WAV overview")
            .detail_context
            .expect("strong WAV context")
    }

    #[test]
    fn exact_span_peak_uses_interleaved_channels_and_half_open_bounds() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("stereo.wav");
        write_float_wav(
            &path,
            2,
            &[[0.1, -0.2], [0.3, -0.9], [0.4, 0.5], [0.8, -0.7]],
        );
        let context = context_for(&path);

        let first = WavSpanPeakRequest::new(
            context.clone(),
            WaveformDetailRange {
                start_frame: 0,
                end_frame: 1,
            },
        )
        .expect("first frame request");
        assert!((load_wav_span_peak(&first).expect("first peak") - 0.2).abs() < 0.000_01);

        let middle = WavSpanPeakRequest::from_frames(context, 1, 3).expect("middle request");
        assert!((load_wav_span_peak(&middle).expect("middle peak") - 0.9).abs() < 0.000_01);
    }

    #[test]
    fn integer_and_float_span_peaks_preserve_raw_sample_magnitude() {
        let directory = TempDir::new().expect("temporary directory");
        let integer_path = directory.path().join("integer.wav");
        let integer_spec = WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut integer = WavWriter::create(&integer_path, integer_spec).expect("integer WAV");
        for sample in [8_192_i16, -16_384, 4_096] {
            integer.write_sample(sample).expect("integer sample");
        }
        integer.finalize().expect("finish integer WAV");
        let integer_request = WavSpanPeakRequest::from_frames(context_for(&integer_path), 0, 3)
            .expect("integer request");
        assert!(
            (load_wav_span_peak(&integer_request).expect("integer peak") - 0.5).abs() < 0.000_01
        );

        let float_path = directory.path().join("float.wav");
        write_float_wav(&float_path, 1, &[[0.25, 0.0], [-0.75, 0.0], [0.5, 0.0]]);
        let float_request =
            WavSpanPeakRequest::from_frames(context_for(&float_path), 0, 3).expect("float request");
        assert!((load_wav_span_peak(&float_request).expect("float peak") - 0.75).abs() < 0.000_01);
    }

    #[test]
    fn silence_and_nonfinite_samples_follow_raw_peak_max() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("nonfinite.wav");
        write_float_wav(&path, 1, &[[f32::NAN, 0.0], [f32::INFINITY, 0.0]]);
        let context = context_for(&path);
        let nan_only = WavSpanPeakRequest::from_frames(context.clone(), 0, 1).unwrap();
        assert_eq!(load_wav_span_peak(&nan_only).unwrap(), 0.0);
        let infinity = WavSpanPeakRequest::from_frames(context, 1, 2).unwrap();
        assert!(load_wav_span_peak(&infinity).unwrap().is_infinite());

        let silence_path = directory.path().join("silence.wav");
        write_float_wav(&silence_path, 1, &[[0.0, 0.0], [-0.0, 0.0]]);
        let silence = WavSpanPeakRequest::from_frames(context_for(&silence_path), 0, 2).unwrap();
        assert_eq!(load_wav_span_peak(&silence).unwrap(), 0.0);
    }

    #[test]
    fn span_peak_rejects_invalid_ranges_without_visible_frame_cap() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("ranges.wav");
        write_float_wav(&path, 1, &[[0.5, 0.0], [0.25, 0.0]]);
        let context = context_for(&path);
        assert!(WavSpanPeakRequest::from_frames(context.clone(), 0, 0).is_err());
        assert!(WavSpanPeakRequest::from_frames(context.clone(), 2, 3).is_err());
        assert!(WavSpanPeakRequest::from_frames(context, 1, 3).is_err());

        let large_path = directory.path().join("large-span.wav");
        let spec = float_spec(1);
        let mut writer = WavWriter::create(&large_path, spec).expect("large WAV");
        for index in 0..(crate::MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES + 1) {
            writer
                .write_sample(if index == crate::MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES {
                    0.75
                } else {
                    0.0
                })
                .expect("large WAV sample");
        }
        writer.finalize().expect("finish large WAV");
        let large_context = context_for(&large_path);
        let large_request = WavSpanPeakRequest::from_frames(
            large_context,
            0,
            crate::MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES + 1,
        )
        .expect("large span request");
        assert!((load_wav_span_peak(&large_request).unwrap() - 0.75).abs() < 0.000_01);
    }

    #[test]
    fn span_peak_rejects_replacement_modification_and_truncation() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("fenced.wav");
        write_float_wav(&path, 1, &[[0.25, 0.0], [0.5, 0.0], [0.75, 0.0]]);
        let context = context_for(&path);
        let request = WavSpanPeakRequest::from_frames(context.clone(), 0, 3).unwrap();

        let replacement = directory.path().join("replacement.wav");
        write_float_wav(&replacement, 1, &[[0.9, 0.0], [0.9, 0.0], [0.9, 0.0]]);
        fs::rename(&replacement, &path).expect("replace WAV source");
        assert!(load_wav_span_peak(&request).is_err());

        write_float_wav(&path, 1, &[[0.25, 0.0], [0.5, 0.0], [0.75, 0.0]]);
        let refreshed = context_for(&path);
        let refreshed_request = WavSpanPeakRequest::from_frames(refreshed, 0, 3).unwrap();
        let mut source = OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open WAV for mutation");
        source.seek(SeekFrom::End(-1)).expect("seek WAV payload");
        source.write_all(&[0x7f]).expect("mutate WAV payload");
        source.flush().expect("flush WAV payload");
        assert!(load_wav_span_peak(&refreshed_request).is_err());

        let truncated_context = context_for(&path);
        let truncated_request = WavSpanPeakRequest::from_frames(truncated_context, 0, 3).unwrap();
        let file_len = fs::metadata(&path).expect("WAV metadata").len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen WAV for truncation")
            .set_len(file_len.saturating_sub(2))
            .expect("truncate WAV");
        assert!(load_wav_span_peak(&truncated_request).is_err());
    }

    #[test]
    fn span_peak_honors_cancellation_during_bounded_read() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("cancel.wav");
        let spec = float_spec(1);
        let mut writer = WavWriter::create(&path, spec).expect("cancel WAV");
        for _ in 0..(PEAK_READ_BLOCK_SAMPLES * 2) {
            writer.write_sample(0.25).expect("cancel sample");
        }
        writer.finalize().expect("finish cancel WAV");

        let mut reader = hound::WavReader::open(&path).expect("open cancel WAV");
        let mut samples = reader.samples::<f32>();
        let cancellation = AtomicBool::new(false);
        let mut sample_count = 0;
        let result = stream_sample_peak(
            &mut samples,
            1,
            PEAK_READ_BLOCK_SAMPLES * 2,
            Some(&cancellation),
            "float",
            |sample| {
                sample_count += 1;
                if sample_count == PEAK_READ_BLOCK_SAMPLES + 1 {
                    cancellation.store(true, Ordering::Release);
                }
                sample
            },
        );
        assert_eq!(result, Err(String::from("WAV span peak canceled")));
    }

    #[test]
    fn span_peak_cancellation_budget_is_interleaved_sample_bounded() {
        let channels = usize::from(u16::MAX);
        let frame_count = 2;
        let mut samples = std::iter::repeat_with(|| Ok::<f32, hound::Error>(0.25));
        let cancellation = AtomicBool::new(false);
        let mut decoded_samples = 0usize;
        let result = stream_sample_peak(
            &mut samples,
            channels,
            frame_count,
            Some(&cancellation),
            "float",
            |sample| {
                decoded_samples += 1;
                if decoded_samples == 3 {
                    cancellation.store(true, Ordering::Release);
                }
                sample
            },
        );
        assert_eq!(result, Err(String::from("WAV span peak canceled")));
        assert!(decoded_samples >= 3);
        assert!(decoded_samples <= PEAK_READ_BLOCK_SAMPLES);
    }
}
