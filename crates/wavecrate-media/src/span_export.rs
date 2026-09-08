//! Exact, bounded-memory WAV export for one frame span.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

use crate::{
    WaveformDetailContext, WaveformDetailRange, WaveformSpec, detail, is_cancelled,
    span_peak::WavSpanPeakRequest,
};

const EXPORT_READ_BLOCK_SAMPLES: usize = 4_096;

/// A file-fenced request for exporting one half-open WAV frame span.
///
/// The exported data retains the source channel count, sample rate, sample format, and bit
/// depth. No visual normalization, downmixing, fading, or other sample processing is applied.
#[derive(Clone, Debug, PartialEq)]
pub struct WavSpanExportRequest {
    /// Header-only revision/spec context used to fence this export.
    pub context: WaveformDetailContext,
    /// Exact half-open source frame range to export.
    pub range: WaveformDetailRange,
}

impl WavSpanExportRequest {
    /// Capture a header-only, file-fenced request for a normalized WAV span.
    ///
    /// `start` and `end` are normalized half-open bounds. The constructor reads WAV metadata and
    /// filesystem identity only; it does not decode or export the span. The returned request can
    /// be passed to [`export_wav_span_with_cancel`] on a later background task.
    pub fn from_path_span_with_cancel(
        path: &Path,
        start: f64,
        end: f64,
        cancellation: Option<&AtomicBool>,
    ) -> Result<Self, String> {
        let request =
            WavSpanPeakRequest::from_path_span_with_cancel(path, start, end, cancellation)
                .map_err(|error| export_constructor_error(error, cancellation))?;
        Self::new(request.context, request.range)
    }

    /// Capture a header-only, file-fenced request for a normalized WAV span without cancellation.
    pub fn from_path_span(path: &Path, start: f64, end: f64) -> Result<Self, String> {
        Self::from_path_span_with_cancel(path, start, end, None)
    }

    /// Validate and create a request for `range` in `context`.
    pub fn new(context: WaveformDetailContext, range: WaveformDetailRange) -> Result<Self, String> {
        let request = WavSpanPeakRequest::new(context, range)
            .map_err(|error| error.replace("WAV span peak", "WAV span export"))?;
        validate_export_spec(request.context.spec)?;
        Ok(Self {
            context: request.context,
            range: request.range,
        })
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
}

fn export_constructor_error(error: String, cancellation: Option<&AtomicBool>) -> String {
    if is_cancelled(cancellation) {
        String::from("WAV span export canceled")
    } else {
        error.replace("WAV span peak", "WAV span export")
    }
}

fn validate_export_spec(spec: WaveformSpec) -> Result<(), String> {
    let supported = matches!(
        (spec.sample_format, spec.bits_per_sample),
        (hound::SampleFormat::Int, 8 | 16 | 24 | 32) | (hound::SampleFormat::Float, 32)
    );
    if !supported {
        return Err(String::from(
            "WAV span export sample format is unsupported; expected 8/16/24/32-bit integer or 32-bit float",
        ));
    }

    let bytes_per_sample = u64::from(spec.bits_per_sample / 8);
    let block_align = u64::from(spec.channels)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| String::from("WAV span export sample layout is too large"))?;
    if block_align == 0 || block_align > u64::from(u16::MAX) {
        return Err(String::from(
            "WAV span export sample layout cannot be represented by WAV",
        ));
    }
    let byte_rate = u64::from(spec.sample_rate)
        .checked_mul(block_align)
        .ok_or_else(|| String::from("WAV span export sample rate is too large"))?;
    if byte_rate > u64::from(u32::MAX) {
        return Err(String::from(
            "WAV span export byte rate cannot be represented by WAV",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceDataLayout {
    data_offset: u64,
    data_len: u64,
    source_block_align: u16,
}

/// Read the RIFF bounds and data chunk before writing any destination bytes.
fn inspect_source_data(
    file: &mut File,
    file_len: u64,
    cancellation: Option<&AtomicBool>,
) -> Result<SourceDataLayout, String> {
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to seek WAV source header: {error}"))?;

    let mut header = [0_u8; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("failed to read WAV source header: {error}"))?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err(String::from("WAV source is not a RIFF/WAVE file"));
    }

    let riff_size = u64::from(u32::from_le_bytes([
        header[4], header[5], header[6], header[7],
    ]));
    let riff_end = 8_u64
        .checked_add(riff_size)
        .ok_or_else(|| String::from("WAV RIFF size overflows source bounds"))?;
    if riff_end < 12 || riff_end > file_len {
        return Err(String::from("WAV RIFF size exceeds source bounds"));
    }

    let mut position = 12_u64;
    let mut data = None;
    let mut source_block_align = None;
    while position < riff_end {
        if is_cancelled(cancellation) {
            return Err(String::from("WAV span export canceled"));
        }
        let remaining = riff_end - position;
        if remaining < 8 {
            return Err(String::from("WAV chunk header exceeds RIFF bounds"));
        }
        file.seek(SeekFrom::Start(position))
            .map_err(|error| format!("failed to seek WAV chunk header: {error}"))?;
        let mut chunk_header = [0_u8; 8];
        file.read_exact(&mut chunk_header)
            .map_err(|error| format!("failed to read WAV chunk header: {error}"))?;
        let chunk_len = u64::from(u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]));
        let payload_offset = position
            .checked_add(8)
            .ok_or_else(|| String::from("WAV chunk offset overflows source bounds"))?;
        let payload_end = payload_offset
            .checked_add(chunk_len)
            .ok_or_else(|| String::from("WAV chunk size overflows source bounds"))?;
        let next_position = payload_end
            .checked_add(chunk_len % 2)
            .ok_or_else(|| String::from("WAV chunk padding overflows source bounds"))?;
        // Hound's ordinary writer leaves a final odd-sized data chunk without the optional
        // RIFF pad byte. Accept that decoder-compatible shape only when the data payload itself
        // reaches the declared RIFF end; all other odd chunks still require their pad byte.
        let next_position =
            if chunk_len % 2 == 1 && payload_end == riff_end && &chunk_header[..4] == b"data" {
                payload_end
            } else {
                next_position
            };
        if next_position > riff_end || next_position > file_len {
            return Err(String::from("WAV chunk size exceeds source bounds"));
        }

        if &chunk_header[..4] == b"data" {
            if data.is_some() {
                return Err(String::from("WAV source contains multiple data chunks"));
            }
            data = Some(SourceDataLayout {
                data_offset: payload_offset,
                data_len: chunk_len,
                source_block_align: 0,
            });
        } else if &chunk_header[..4] == b"fmt " {
            if chunk_len < 16 {
                return Err(String::from(
                    "WAV fmt chunk is shorter than its fixed header",
                ));
            }
            file.seek(SeekFrom::Start(payload_offset))
                .map_err(|error| format!("failed to seek WAV fmt chunk: {error}"))?;
            let mut fmt_header = [0_u8; 16];
            file.read_exact(&mut fmt_header)
                .map_err(|error| format!("failed to read WAV fmt chunk: {error}"))?;
            source_block_align = Some(u16::from_le_bytes([fmt_header[12], fmt_header[13]]));
        }
        position = next_position;
    }
    if position != riff_end {
        return Err(String::from("WAV RIFF chunks do not fit declared bounds"));
    }
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }
    let mut data = data.ok_or_else(|| String::from("WAV source has no data chunk"))?;
    data.source_block_align =
        source_block_align.ok_or_else(|| String::from("WAV source has no fmt chunk"))?;
    Ok(data)
}

fn checked_span_layout(
    request: &WavSpanExportRequest,
    opened: &detail::OpenWav,
    source_data: SourceDataLayout,
) -> Result<(u64, u32), String> {
    let spec = opened.spec;
    let bytes_per_sample = u64::from(spec.bits_per_sample / 8);
    let block_align = u64::from(spec.channels)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| String::from("WAV span export sample layout is too large"))?;
    if u64::from(source_data.source_block_align) != block_align {
        return Err(String::from(
            "WAV source sample container layout is unsupported for exact export",
        ));
    }
    let total_frames = u64::try_from(opened.total_frames)
        .map_err(|_| String::from("WAV span export source frame count is too large"))?;
    let expected_source_data_len = total_frames
        .checked_mul(block_align)
        .ok_or_else(|| String::from("WAV source data size overflows frame bounds"))?;
    if source_data.data_len != expected_source_data_len {
        return Err(String::from(
            "WAV source data size does not match its declared frame count",
        ));
    }

    let start_frame = u64::try_from(request.range.start_frame)
        .map_err(|_| String::from("WAV span export starts too late"))?;
    let frame_count = u64::try_from(request.range.len())
        .map_err(|_| String::from("WAV span export is too large"))?;
    let start_data_byte = start_frame
        .checked_mul(block_align)
        .ok_or_else(|| String::from("WAV span export starts too late"))?;
    let data_len = frame_count
        .checked_mul(block_align)
        .ok_or_else(|| String::from("WAV span export is too large"))?;
    let end_data_byte = start_data_byte
        .checked_add(data_len)
        .ok_or_else(|| String::from("WAV span export is too large"))?;
    if end_data_byte > source_data.data_len {
        return Err(String::from("WAV span export exceeds source data"));
    }
    source_data
        .data_offset
        .checked_add(end_data_byte)
        .ok_or_else(|| String::from("WAV span export source offset overflows"))?;

    let output_data_len = u32::try_from(data_len)
        .map_err(|_| String::from("WAV span export is too large for RIFF output"))?;
    let output_header_payload = if spec.channels > 2 || spec.bits_per_sample > 16 {
        60_u64
    } else {
        36_u64
    };
    output_header_payload
        .checked_add(data_len)
        .filter(|size| *size <= u64::from(u32::MAX))
        .ok_or_else(|| String::from("WAV span export is too large for RIFF output"))?;
    Ok((data_len, output_data_len))
}

/// Export one exact WAV frame span to a caller-owned sink.
///
/// `sink` must be a fresh `Write + Seek` destination positioned at offset zero and must not
/// refer to the source file. The generic sink API cannot verify filesystem aliasing. The exporter may
/// leave arbitrary partial bytes in the sink after cancellation or any error; callers must discard
/// that sink and provide a fresh sink for a retry. This function writes and finalizes bytes only;
/// it does not create, rename, publish, or atomically replace a filesystem destination.
pub fn export_wav_span<W: Write + Seek>(
    request: &WavSpanExportRequest,
    sink: &mut W,
) -> Result<(), String> {
    export_wav_span_with_cancel(request, sink, None)
}

/// Export one exact WAV frame span to a caller-owned sink while honoring cancellation.
///
/// Cancellation is checked before writing, after each block of at most 4,096 interleaved samples,
/// and after reading the requested span. The sink remains caller-owned and may contain arbitrary
/// partial output when this function returns an error.
pub fn export_wav_span_with_cancel<W: Write + Seek>(
    request: &WavSpanExportRequest,
    sink: &mut W,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }
    WavSpanPeakRequest::new(request.context.clone(), request.range)
        .map_err(|error| error.replace("WAV span peak", "WAV span export"))?;
    validate_export_spec(request.context.spec)?;

    let context = &request.context;
    let mut opened = detail::open_wav(&context.path, cancellation).map_err(|error| {
        if is_cancelled(cancellation) {
            String::from("WAV span export canceled")
        } else {
            format!(
                "WAV span export failed for {}: {error}",
                context.path.display()
            )
        }
    })?;
    if detail::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
    {
        return Err(String::from("stale WAV span export request"));
    }
    if opened.observation.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV span export request"));
    }

    let source_data = inspect_source_data(
        &mut opened.file,
        opened.observation.byte_len(),
        cancellation,
    )?;
    let (_data_len, output_data_len) = checked_span_layout(request, &opened, source_data)?;
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind WAV span export source: {error}"))?;
    let reader_file = opened
        .file
        .try_clone()
        .map_err(|error| format!("failed to retain WAV span export source: {error}"))?;
    opened.reader = hound::WavReader::new(std::io::BufReader::new(reader_file))
        .map_err(|error| format!("failed to reopen WAV span export source: {error}"))?;
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }

    let after_header =
        detail::recheck_wav(&context.path, &opened, cancellation).map_err(|error| {
            if is_cancelled(cancellation) {
                String::from("WAV span export canceled")
            } else {
                format!("stale WAV span export request: {error}")
            }
        })?;
    if after_header.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV span export request"));
    }
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }

    sink.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to seek WAV span export sink: {error}"))?;
    let mut writer = hound::WavWriter::new(sink, opened.spec)
        .map_err(|error| format!("failed to initialize WAV span export: {error}"))?;
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }

    opened
        .reader
        .seek(u32::try_from(request.range.start_frame).map_err(|_| {
            String::from("WAV span export range cannot be represented by WAV reader")
        })?)
        .map_err(|error| format!("failed to seek WAV span export source: {error}"))?;
    copy_wav_samples(
        &mut opened.reader,
        opened.spec,
        request.range.len(),
        &mut writer,
        cancellation,
    )?;

    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }
    let after_read =
        detail::recheck_wav(&context.path, &opened, cancellation).map_err(|error| {
            if is_cancelled(cancellation) {
                String::from("WAV span export canceled")
            } else {
                format!("stale WAV span export result: {error}")
            }
        })?;
    detail::ensure_file_unchanged(&opened.observation, &after_read)
        .map_err(|_| String::from("stale WAV span export result"))?;
    if after_read.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV span export result"));
    }
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }

    if writer.len() != output_data_len / u32::from(opened.spec.bits_per_sample / 8) {
        return Err(String::from(
            "WAV span export wrote an incomplete sample span",
        ));
    }
    writer
        .finalize()
        .map_err(|error| format!("failed to finalize WAV span export: {error}"))
}

fn copy_wav_samples<W: Write + Seek>(
    reader: &mut hound::WavReader<std::io::BufReader<File>>,
    spec: hound::WavSpec,
    frame_count: usize,
    writer: &mut hound::WavWriter<&mut W>,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    let sample_count = frame_count
        .checked_mul(usize::from(spec.channels))
        .ok_or_else(|| String::from("WAV span export sample count is too large"))?;
    match spec.sample_format {
        hound::SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            copy_samples(&mut samples, sample_count, writer, cancellation, "float")
        }
        hound::SampleFormat::Int if spec.bits_per_sample == 8 => {
            let mut samples = reader.samples::<i8>();
            copy_samples(&mut samples, sample_count, writer, cancellation, "integer")
        }
        hound::SampleFormat::Int if spec.bits_per_sample == 16 => {
            let mut samples = reader.samples::<i16>();
            copy_samples(&mut samples, sample_count, writer, cancellation, "integer")
        }
        hound::SampleFormat::Int => {
            let mut samples = reader.samples::<i32>();
            copy_samples(&mut samples, sample_count, writer, cancellation, "integer")
        }
    }
}

fn copy_samples<T, I, W>(
    samples: &mut I,
    sample_count: usize,
    writer: &mut hound::WavWriter<&mut W>,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
) -> Result<(), String>
where
    T: hound::Sample,
    I: Iterator<Item = Result<T, hound::Error>>,
    W: Write + Seek,
{
    for sample_index in 0..sample_count {
        if sample_index.is_multiple_of(EXPORT_READ_BLOCK_SAMPLES) && is_cancelled(cancellation) {
            return Err(String::from("WAV span export canceled"));
        }
        let sample = samples
            .next()
            .transpose()
            .map_err(|error| format!("failed to read {sample_kind} WAV sample: {error}"))?
            .ok_or_else(|| String::from("WAV ended before declared frame count"))?;
        writer
            .write_sample(sample)
            .map_err(|error| format!("failed to write WAV span sample: {error}"))?;
        if (sample_index + 1).is_multiple_of(EXPORT_READ_BLOCK_SAMPLES)
            && is_cancelled(cancellation)
        {
            return Err(String::from("WAV span export canceled"));
        }
    }
    if is_cancelled(cancellation) {
        return Err(String::from("WAV span export canceled"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::{
        fs::{self, OpenOptions},
        io::{Cursor, Error, ErrorKind, Seek, SeekFrom},
        sync::atomic::{AtomicBool, Ordering},
    };
    use tempfile::TempDir;

    fn write_int_wav(path: &Path, spec: WavSpec, samples: &[i32]) {
        let mut writer = WavWriter::create(path, spec).expect("create source WAV");
        for sample in samples {
            writer.write_sample(*sample).expect("write source sample");
        }
        writer.finalize().expect("finish source WAV");
    }

    fn write_float_wav(path: &Path, spec: WavSpec, samples: &[f32]) {
        let mut writer = WavWriter::create(path, spec).expect("create source WAV");
        for sample in samples {
            writer.write_sample(*sample).expect("write source sample");
        }
        writer.finalize().expect("finish source WAV");
    }

    fn context_for(path: &Path) -> WaveformDetailContext {
        crate::load_waveform_preview(path)
            .expect("WAV overview")
            .detail_context
            .expect("strong WAV context")
    }

    #[test]
    fn exports_exact_stereo_half_open_span_and_preserves_format() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("stereo.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, -2, 300, -400, 500, -600, 700, -800]);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 1, 3)
            .expect("span export request");
        let mut output = Cursor::new(Vec::new());
        export_wav_span(&request, &mut output).expect("span export");

        output.set_position(0);
        let mut reader = hound::WavReader::new(output).expect("read exported WAV");
        assert_eq!(reader.spec(), spec);
        assert_eq!(reader.duration(), 2);
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, vec![300, -400, 500, -600]);
    }

    #[test]
    fn normalized_constructor_rounds_outward_to_frames() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("normalized.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[10, 20, 30, 40, 50]);
        let request =
            WavSpanExportRequest::from_path_span(&path, 0.21, 0.61).expect("normalized request");
        assert_eq!(request.range.start_frame, 1);
        assert_eq!(request.range.end_frame, 4);
    }

    #[test]
    fn preserves_24_bit_integer_and_float_bits() {
        let directory = TempDir::new().expect("temporary directory");
        let int_path = directory.path().join("pcm24.wav");
        let int_spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        let int_samples = [8_388_607, -8_388_608, 0x0012_3456];
        write_int_wav(&int_path, int_spec, &int_samples);
        let int_request = WavSpanExportRequest::from_frames(context_for(&int_path), 0, 3)
            .expect("24-bit request");
        let mut int_output = Cursor::new(Vec::new());
        export_wav_span(&int_request, &mut int_output).expect("24-bit export");
        int_output.set_position(0);
        let mut int_reader = hound::WavReader::new(int_output).expect("read 24-bit export");
        assert_eq!(int_reader.spec(), int_spec);
        assert_eq!(
            int_reader
                .samples::<i32>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            int_samples
        );

        let float_path = directory.path().join("float.wav");
        let float_spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let float_samples = [
            f32::from_bits(0x7fc1_2345),
            f32::from_bits(0x8000_0000),
            0.25_f32,
        ];
        write_float_wav(&float_path, float_spec, &float_samples);
        let float_request = WavSpanExportRequest::from_frames(context_for(&float_path), 0, 3)
            .expect("float request");
        let mut float_output = Cursor::new(Vec::new());
        export_wav_span(&float_request, &mut float_output).expect("float export");
        float_output.set_position(0);
        let mut float_reader = hound::WavReader::new(float_output).expect("read float export");
        let exported = float_reader
            .samples::<f32>()
            .map(|sample| sample.expect("float sample").to_bits())
            .collect::<Vec<_>>();
        assert_eq!(
            exported,
            float_samples
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn long_nonzero_span_reads_after_source_header_scan() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("long.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let frame_count = 5_000;
        let mut samples = Vec::with_capacity(frame_count * 2);
        for frame in 0..frame_count {
            samples.push(i32::try_from(frame).expect("frame fits") + 1);
            samples.push(-i32::try_from(frame).expect("frame fits") - 1);
        }
        write_int_wav(&path, spec, &samples);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 3_001, 3_005)
            .expect("long span request");
        let mut output = Cursor::new(Vec::new());
        export_wav_span(&request, &mut output).expect("long span export");
        output.set_position(0);
        let mut reader = hound::WavReader::new(output).expect("read long span export");
        assert_eq!(reader.spec(), spec);
        assert_eq!(reader.duration(), 4);
        assert_eq!(
            reader
                .samples::<i16>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![3_002, -3_002, 3_003, -3_003, 3_004, -3_004, 3_005, -3_005]
        );
    }

    #[test]
    fn odd_sized_8_bit_hound_data_chunk_roundtrips() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("8bit.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 8,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, -2, 127]);
        let request =
            WavSpanExportRequest::from_frames(context_for(&path), 0, 3).expect("8-bit request");
        let mut output = Cursor::new(Vec::new());
        export_wav_span(&request, &mut output).expect("8-bit export");
        let bytes = output.into_inner();
        assert_eq!(bytes.len(), 47);
        assert_eq!(&bytes[4..8], &(39_u32.to_le_bytes()));
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).expect("read 8-bit export");
        assert_eq!(reader.spec(), spec);
        assert_eq!(
            reader
                .samples::<i8>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![1, -2, 127]
        );
    }

    #[test]
    fn malformed_riff_size_is_rejected_before_sink_writes() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("bad-riff.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        let mut bytes = fs::read(&path).expect("read source WAV");
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, bytes).expect("write malformed source");
        let request = WavSpanExportRequest::from_path_span(&path, 0.0, 1.0)
            .expect("malformed source context");
        let mut output = Cursor::new(Vec::new());
        let error = export_wav_span(&request, &mut output).expect_err("malformed RIFF");
        assert!(
            error.contains("RIFF") || error.contains("bounds"),
            "{error}"
        );
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn output_riff_size_overflow_is_rejected_without_allocating_payload() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("overflow.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1]);
        let mut opened = detail::open_wav(&path, None).expect("open source WAV");
        let huge_frames = usize::try_from(u32::MAX / 4).expect("32-bit frame count");
        opened.total_frames = huge_frames;
        let mut context = context_for(&path);
        context.total_frames = huge_frames;
        let request = WavSpanExportRequest {
            context,
            range: WaveformDetailRange {
                start_frame: 0,
                end_frame: huge_frames,
            },
        };
        let source_data = SourceDataLayout {
            data_offset: 44,
            data_len: u64::from(u32::MAX - 3),
            source_block_align: 4,
        };
        let error =
            checked_span_layout(&request, &opened, source_data).expect_err("RIFF output overflow");
        assert!(error.contains("too large for RIFF"), "{error}");
    }

    #[test]
    fn cancellation_is_checked_in_bounded_interleaved_blocks() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("cancel.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let samples = vec![123_i32; EXPORT_READ_BLOCK_SAMPLES * 2 + 2];
        write_int_wav(&path, spec, &samples);
        let request =
            WavSpanExportRequest::from_frames(context_for(&path), 0, EXPORT_READ_BLOCK_SAMPLES + 1)
                .expect("cancel request");
        let cancellation = AtomicBool::new(false);
        let mut output = CancelAfterWrites::new(&cancellation, 44 + EXPORT_READ_BLOCK_SAMPLES * 2);
        let error = export_wav_span_with_cancel(&request, &mut output, Some(&cancellation))
            .expect_err("cancel export");
        assert_eq!(error, "WAV span export canceled");
    }

    #[test]
    fn stale_source_is_rejected_before_sink_writes() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("stale.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        let request =
            WavSpanExportRequest::from_frames(context_for(&path), 0, 3).expect("stale request");
        let replacement = directory.path().join("replacement.wav");
        write_int_wav(&replacement, spec, &[4, 5, 6]);
        fs::rename(replacement, &path).expect("replace source");
        let mut output = Cursor::new(Vec::new());
        let error = export_wav_span(&request, &mut output).expect_err("stale source");
        assert!(
            error.contains("stale") || error.contains("replaced"),
            "{error}"
        );
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn sink_write_seek_and_finalize_errors_are_reported() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("sink-errors.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 0, 3)
            .expect("sink error request");

        let mut write_failure = FailingSink::write_failure();
        let error = export_wav_span(&request, &mut write_failure).expect_err("sink write error");
        assert!(
            error.contains("initialize") || error.contains("write"),
            "{error}"
        );

        let mut seek_failure = FailingSink::seek_failure_on(2);
        let error = export_wav_span(&request, &mut seek_failure).expect_err("sink seek error");
        assert!(
            error.contains("finalize") || error.contains("seek"),
            "{error}"
        );

        let mut flush_failure = FailingSink::flush_failure();
        let error = export_wav_span(&request, &mut flush_failure).expect_err("sink flush error");
        assert!(
            error.contains("finalize") || error.contains("flush"),
            "{error}"
        );
    }

    #[test]
    fn source_change_after_writing_begins_is_rejected_before_finalize() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("postread-stale.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 0, 3)
            .expect("postread stale request");
        let mut output = MutateSourceSink::new(&path);
        let error = export_wav_span(&request, &mut output).expect_err("postread stale source");
        #[cfg(windows)]
        {
            assert!(output.mutation_denied);
            assert!(
                error.contains("source mutation") || error.contains("denied"),
                "{error}"
            );
            OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("source read lease must release after export");
        }
        #[cfg(not(windows))]
        assert!(
            error.contains("stale") || error.contains("changed"),
            "{error}"
        );
        assert!(!output.inner.get_ref().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn successful_export_releases_source_read_lease() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("read-lease.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 0, 3)
            .expect("read lease request");
        let mut output = Cursor::new(Vec::new());
        export_wav_span(&request, &mut output).expect("successful export");

        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("source read lease must release after successful export");
    }

    #[cfg(windows)]
    #[test]
    fn replacement_after_export_starts_is_rejected_by_retained_identity() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("replacement-source.wav");
        let replacement = directory.path().join("replacement.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        write_int_wav(&path, spec, &[1, 2, 3]);
        write_int_wav(&replacement, spec, &[4, 5, 6]);
        let request = WavSpanExportRequest::from_frames(context_for(&path), 0, 3)
            .expect("replacement request");
        let mut output = ReplaceSourceSink::new(&path, &replacement);
        let error = export_wav_span(&request, &mut output).expect_err("replacement must stale");

        assert!(output.replaced, "replacement rename must succeed");
        assert!(path.is_file(), "replacement must occupy the source path");
        assert!(!replacement.exists(), "replacement path must be consumed");
        assert!(
            error.contains("stale") || error.contains("replaced"),
            "{error}"
        );
    }

    struct CancelAfterWrites<'a> {
        cancellation: &'a AtomicBool,
        write_calls: usize,
        cancel_after: usize,
        bytes: Vec<u8>,
        position: u64,
    }

    impl<'a> CancelAfterWrites<'a> {
        fn new(cancellation: &'a AtomicBool, cancel_after: usize) -> Self {
            Self {
                cancellation,
                write_calls: 0,
                cancel_after,
                bytes: Vec::new(),
                position: 0,
            }
        }
    }

    impl Write for CancelAfterWrites<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.write_calls += bytes.len();
            self.bytes.extend_from_slice(bytes);
            self.position = self.position.saturating_add(bytes.len() as u64);
            if self.write_calls >= self.cancel_after {
                self.cancellation.store(true, Ordering::Release);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for CancelAfterWrites<'_> {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            let next = match position {
                SeekFrom::Start(offset) => i128::from(offset),
                SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
                SeekFrom::End(offset) => i128::from(self.bytes.len() as u64) + i128::from(offset),
            };
            if next < 0 {
                return Err(Error::new(ErrorKind::InvalidInput, "negative seek"));
            }
            self.position = u64::try_from(next)
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "seek overflow"))?;
            Ok(self.position)
        }
    }

    struct FailingSink {
        bytes: Vec<u8>,
        position: u64,
        fail_writes: bool,
        fail_seek_at: Option<usize>,
        fail_flush: bool,
        seek_calls: usize,
    }

    impl FailingSink {
        fn write_failure() -> Self {
            Self {
                bytes: Vec::new(),
                position: 0,
                fail_writes: true,
                fail_seek_at: None,
                fail_flush: false,
                seek_calls: 0,
            }
        }

        fn seek_failure_on(seek_call: usize) -> Self {
            Self {
                bytes: Vec::new(),
                position: 0,
                fail_writes: false,
                fail_seek_at: Some(seek_call),
                fail_flush: false,
                seek_calls: 0,
            }
        }

        fn flush_failure() -> Self {
            Self {
                bytes: Vec::new(),
                position: 0,
                fail_writes: false,
                fail_seek_at: None,
                fail_flush: true,
                seek_calls: 0,
            }
        }
    }

    impl Write for FailingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_writes {
                return Err(Error::new(ErrorKind::Other, "injected sink write failure"));
            }
            self.bytes.extend_from_slice(bytes);
            self.position = self.position.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(Error::new(ErrorKind::Other, "injected sink flush failure"))
            } else {
                Ok(())
            }
        }
    }

    impl Seek for FailingSink {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.seek_calls += 1;
            if self.fail_seek_at == Some(self.seek_calls) {
                return Err(Error::new(ErrorKind::Other, "injected sink seek failure"));
            }
            let next = match position {
                SeekFrom::Start(offset) => i128::from(offset),
                SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
                SeekFrom::End(offset) => i128::from(self.bytes.len() as u64) + i128::from(offset),
            };
            if next < 0 {
                return Err(Error::new(ErrorKind::InvalidInput, "negative seek"));
            }
            self.position = u64::try_from(next)
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "seek overflow"))?;
            Ok(self.position)
        }
    }

    struct MutateSourceSink {
        inner: Cursor<Vec<u8>>,
        source: std::path::PathBuf,
        writes: usize,
        mutation_denied: bool,
    }

    impl MutateSourceSink {
        fn new(source: &Path) -> Self {
            Self {
                inner: Cursor::new(Vec::new()),
                source: source.to_path_buf(),
                writes: 0,
                mutation_denied: false,
            }
        }
    }

    impl Write for MutateSourceSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            let written = self.inner.write(bytes)?;
            if self.writes == 2 {
                let mut source = match OpenOptions::new().write(true).open(&self.source) {
                    Ok(source) => source,
                    Err(error) => {
                        self.mutation_denied = true;
                        return Err(Error::new(
                            error.kind(),
                            format!("source mutation denied during export: {error}"),
                        ));
                    }
                };
                source.seek(SeekFrom::End(-1))?;
                source.write_all(&[0x7f])?;
                source.flush()?;
            }
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for MutateSourceSink {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[cfg(windows)]
    struct ReplaceSourceSink {
        inner: Cursor<Vec<u8>>,
        source: std::path::PathBuf,
        replacement: std::path::PathBuf,
        writes: usize,
        replaced: bool,
    }

    #[cfg(windows)]
    impl ReplaceSourceSink {
        fn new(source: &Path, replacement: &Path) -> Self {
            Self {
                inner: Cursor::new(Vec::new()),
                source: source.to_path_buf(),
                replacement: replacement.to_path_buf(),
                writes: 0,
                replaced: false,
            }
        }
    }

    #[cfg(windows)]
    impl Write for ReplaceSourceSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            let written = self.inner.write(bytes)?;
            if self.writes == 2 {
                fs::rename(&self.replacement, &self.source)?;
                self.replaced = true;
            }
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    #[cfg(windows)]
    impl Seek for ReplaceSourceSink {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }
}
