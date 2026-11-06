//! Bounded PCM input for the tempo preparation worker.

use std::{
    io::{BufReader, Cursor, ErrorKind, Read, Seek, SeekFrom},
    ops::Range,
    sync::atomic::AtomicBool,
    time::Instant,
};

use hound::{Sample, SampleFormat};

use crate::time_stretch::{
    MAX_TEMPO_INPUT_SAMPLES, MAX_TEMPO_SPAN_SECONDS, StretchPreparationError,
    preparation_checkpoint,
};

const MAX_WAV_HEADER_BYTES: u64 = 64 * 1024;
const CHECK_INTERVAL_SAMPLES: usize = 4_096;

/// Exact decoded input for tempo audition.
#[derive(Debug)]
pub struct TempoWavSpan {
    /// Interleaved normalized PCM in the source channel order.
    pub samples: Vec<f32>,
    /// Exact source coordinates of the decoded frames.
    pub source_span: Range<u64>,
    /// Total frames in the source container.
    pub source_frames: u64,
    /// Source sample rate in hertz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: usize,
}

/// Why a WAV span cannot be used for tempo preparation.
#[derive(Debug)]
pub enum TempoWavReadError {
    /// The header, data, or underlying read was invalid or unsupported.
    InvalidWav(String),
    /// The requested range is empty or outside the WAV data.
    InvalidSpan,
    /// The range exceeds the tempo worker's duration or sample budget.
    InputTooLarge,
    /// Cancellation or expiry interrupted preparation.
    Interrupted(StretchPreparationError),
}

/// Read one exact half-open span from a caller-retained WAV source on a worker.
///
/// The caller owns file identity validation and keeps the raw source for fallback.
/// The header scan is limited to 64 KiB and sample reads stop at the requested end frame.
/// The deadline is cooperative and is checked between bounded reads.
pub fn read_tempo_wav_span(
    source: &mut (impl Read + Seek),
    source_span: Range<u64>,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<TempoWavSpan, TempoWavReadError> {
    let checkpoint =
        || preparation_checkpoint(cancel, deadline).map_err(TempoWavReadError::Interrupted);
    checkpoint()?;
    if source_span.start >= source_span.end {
        return Err(TempoWavReadError::InvalidSpan);
    }

    let file_len = source.seek(SeekFrom::End(0)).map_err(invalid_wav)?;
    source.rewind().map_err(invalid_wav)?;
    let mut prefix = vec![0; file_len.min(MAX_WAV_HEADER_BYTES) as usize];
    let mut prefix_read = 0;
    while prefix_read < prefix.len() {
        checkpoint()?;
        match source.read(&mut prefix[prefix_read..]) {
            Ok(0) => break,
            Ok(count) => prefix_read += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(invalid_wav(error)),
        }
    }
    prefix.truncate(prefix_read);
    checkpoint()?;
    let riff_end = hound::read_wave_header(&mut Cursor::new(&prefix)).map_err(invalid_wav)?;
    let header = hound::WavReader::new(Cursor::new(&prefix)).map_err(invalid_wav)?;
    let spec = header.spec();
    let source_frames = u64::from(header.duration());
    let sample_count = u64::from(header.len());
    let data_start = header.into_inner().position();
    // Hound stops just after the data chunk's length. Check the stored layout and extent
    // before calculating a byte offset or allocating a decoded buffer.
    let length_offset = data_start as usize - 4;
    let mut data_size = [0; 4];
    data_size.copy_from_slice(&prefix[length_offset..data_start as usize]);
    let data_bytes = u64::from(u32::from_le_bytes(data_size));
    if riff_end > file_len || data_start + data_bytes > riff_end || sample_count == 0 {
        return Err(invalid_wav("inconsistent WAV data extent"));
    }
    let sample_bytes = (data_bytes / sample_count) as u16;
    let bits = spec.bits_per_sample;
    if spec.sample_rate == 0
        || spec.channels.checked_mul(sample_bytes)
            != crate::wav_sanitize::wav_prefix_block_alignment(&prefix[..length_offset - 4])
        || !matches!(
            (spec.sample_format, sample_bytes, bits),
            (SampleFormat::Int, 1, 8)
                | (SampleFormat::Int, 2, 16)
                | (SampleFormat::Int, 3, 24)
                | (SampleFormat::Int | SampleFormat::Float, 4, 32)
        )
    {
        return Err(invalid_wav("unsupported WAV sample layout"));
    }
    if source_span.end > source_frames {
        return Err(TempoWavReadError::InvalidSpan);
    }

    let frame_count = source_span.end - source_span.start;
    let channels = u64::from(spec.channels);
    let span_samples = frame_count * channels;
    if frame_count > u64::from(spec.sample_rate) * u64::from(MAX_TEMPO_SPAN_SECONDS)
        || span_samples > MAX_TEMPO_INPUT_SAMPLES as u64
    {
        return Err(TempoWavReadError::InputTooLarge);
    }

    checkpoint()?;
    let byte_offset = data_start + source_span.start * channels * u64::from(sample_bytes);
    source
        .seek(SeekFrom::Start(byte_offset))
        .map_err(invalid_wav)?;
    let mut reader = BufReader::new(source.take(span_samples * u64::from(sample_bytes)));
    let mut samples = Vec::with_capacity(span_samples as usize);
    match spec.sample_format {
        SampleFormat::Int => {
            let scale = (1_u64 << (bits - 1)) as f32;
            read_samples(&mut samples, span_samples as usize, checkpoint, || {
                i32::read(&mut reader, SampleFormat::Int, sample_bytes, bits)
                    .map(|sample| sample as f32 / scale)
            })?;
        }
        SampleFormat::Float => {
            read_samples(&mut samples, span_samples as usize, checkpoint, || {
                f32::read(&mut reader, SampleFormat::Float, sample_bytes, bits)
            })?;
        }
    }
    Ok(TempoWavSpan {
        samples,
        source_span,
        source_frames,
        sample_rate: spec.sample_rate,
        channels: channels as usize,
    })
}

fn read_samples(
    samples: &mut Vec<f32>,
    count: usize,
    checkpoint: impl Fn() -> Result<(), TempoWavReadError>,
    mut next: impl FnMut() -> hound::Result<f32>,
) -> Result<(), TempoWavReadError> {
    for block in (0..count).step_by(CHECK_INTERVAL_SAMPLES) {
        checkpoint()?;
        for _ in block..(block + CHECK_INTERVAL_SAMPLES).min(count) {
            let sample = next().map_err(invalid_wav)?;
            if !sample.is_finite() {
                return Err(invalid_wav("non-finite WAV sample"));
            }
            samples.push(sample.clamp(-1.0, 1.0));
        }
    }
    checkpoint()
}

fn invalid_wav(error: impl std::fmt::Display) -> TempoWavReadError {
    TempoWavReadError::InvalidWav(error.to_string())
}

#[cfg(test)]
mod tests;
