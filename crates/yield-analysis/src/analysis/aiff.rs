use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use aifc::{AifcError, AifcReader, FileFormat, Sample};

use super::audio::AudioProbe;
use super::audio_decode::{AnalysisDecodeError, DecodedAudio};

const MAX_HEADER_READ_BYTES: usize = 64 * 1024;

pub(super) fn is_aiff_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("aif") || extension.eq_ignore_ascii_case("aiff")
        })
}

struct PcmAiff {
    reader: AifcReader<HeaderBudget>,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u32,
    frames: u64,
}

impl PcmAiff {
    fn open(path: &Path) -> Result<Self, AnalysisDecodeError> {
        let file = File::open(path).map_err(|error| io_error(path, error))?;
        let file_len = file
            .metadata()
            .map_err(|error| io_error(path, error))?
            .len();
        let mut reader = AifcReader::new(HeaderBudget {
            stream: BufReader::new(file),
            remaining: Some(MAX_HEADER_READ_BYTES),
        })
        .map_err(|error| read_error(path, error))?;
        reader.get_mut().remaining = None;
        let info = reader.info();
        if info.file_format != FileFormat::Aiff
            || !matches!(info.comm_sample_size, 8 | 16 | 24 | 32)
        {
            return Err(AnalysisDecodeError::Unsupported(format!(
                "{} requires 8-, 16-, 24-, or 32-bit uncompressed AIFF PCM",
                path.display()
            )));
        }
        if !(1.0..=f64::from(u32::MAX)).contains(&info.sample_rate)
            || info.sample_rate.fract() != 0.0
        {
            return Err(AnalysisDecodeError::Invalid(format!(
                "{} has an invalid AIFF sample rate",
                path.display()
            )));
        }

        let channels = info.channels as u16;
        let bits_per_sample = info.comm_sample_size as u32;
        let frames = u64::from(info.comm_num_sample_frames);
        let expected_bytes = frames * u64::from(channels) * u64::from(bits_per_sample / 8);
        let data_start = reader
            .get_mut()
            .stream_position()
            .map_err(|error| io_error(path, error))?;
        if frames == 0
            || expected_bytes != u64::from(info.sample_byte_len)
            || expected_bytes > file_len.saturating_sub(data_start)
        {
            return Err(AnalysisDecodeError::Corrupt(format!(
                "{} has missing, incomplete, or inconsistent AIFF sample data",
                path.display()
            )));
        }

        Ok(Self {
            reader,
            sample_rate: info.sample_rate as u32,
            channels,
            bits_per_sample,
            frames,
        })
    }
}

// Seek past chunk payloads while bounding the total bytes the parser can read
// during discovery. Decoding is limited by the validated frame count instead.
struct HeaderBudget {
    stream: BufReader<File>,
    remaining: Option<usize>,
}

impl Read for HeaderBudget {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let Some(remaining) = &mut self.remaining else {
            return self.stream.read(bytes);
        };
        if *remaining == 0 && !bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "AIFF header exceeds read budget",
            ));
        }
        let requested = bytes.len().min(*remaining);
        let read = self.stream.read(&mut bytes[..requested])?;
        *remaining -= read;
        Ok(read)
    }
}

impl Seek for HeaderBudget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.stream.seek(position)
    }
}

pub(super) fn probe(path: &Path) -> Result<AudioProbe, AnalysisDecodeError> {
    let audio = PcmAiff::open(path)?;
    Ok(AudioProbe {
        duration_seconds: Some((audio.frames as f64 / f64::from(audio.sample_rate)) as f32),
        sample_rate: Some(audio.sample_rate),
        channels: Some(audio.channels),
        bits_per_sample: Some(audio.bits_per_sample),
    })
}

pub(super) fn decode(
    path: &Path,
    max_seconds: Option<f32>,
) -> Result<DecodedAudio, AnalysisDecodeError> {
    let mut audio = PcmAiff::open(path)?;
    let frames = max_seconds
        .filter(|limit| *limit > 0.0)
        .map(|limit| {
            let limited = (f64::from(limit) * f64::from(audio.sample_rate)).ceil() as u64;
            limited.min(audio.frames)
        })
        .unwrap_or(audio.frames);
    let mut samples = Vec::new();
    for _ in 0..frames * u64::from(audio.channels) {
        let sample = audio
            .reader
            .read_sample()
            .map_err(|error| read_error(path, error))?;
        let value = match sample {
            Some(Sample::I8(value)) => f32::from(value) / 128.0,
            Some(Sample::I16(value)) => f32::from(value) / 32_768.0,
            Some(Sample::I24(value)) => value as f32 / 8_388_608.0,
            Some(Sample::I32(value)) => value as f32 / 2_147_483_648.0,
            _ => {
                return Err(AnalysisDecodeError::Corrupt(format!(
                    "{} contains unexpected AIFF sample data",
                    path.display()
                )));
            }
        };
        samples.push(value);
    }
    Ok(DecodedAudio {
        samples,
        sample_rate: audio.sample_rate,
        channels: audio.channels,
    })
}

fn read_error(path: &Path, error: AifcError) -> AnalysisDecodeError {
    let detail = format!("AIFF read failed for {}: {error:?}", path.display());
    match error {
        AifcError::Unsupported | AifcError::UnrecognizedFormat => {
            AnalysisDecodeError::Unsupported(detail)
        }
        AifcError::StdIoError(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            io_error(path, error)
        }
        _ => AnalysisDecodeError::Corrupt(detail),
    }
}

fn io_error(path: &Path, error: io::Error) -> AnalysisDecodeError {
    AnalysisDecodeError::Io {
        kind: error.kind(),
        detail: format!("AIFF I/O failed for {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{audio::probe_metadata, audio_decode::decode_audio_typed};

    // Two stereo frames with signed 16-bit big-endian PCM at 48 kHz.
    const FIXTURE: &[u8] = b"FORM\0\0\0\x36AIFFCOMM\0\0\0\x12\0\x02\0\0\0\x02\0\x10\x40\x0e\xbb\x80\0\0\0\0\0\0SSND\0\0\0\x10\0\0\0\0\0\0\0\0\x80\0\x40\0\0\0\xc0\0";

    #[test]
    fn aiff_metadata_and_decode_preserve_big_endian_stereo_frames() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["clip.aiff", "clip.AIF"] {
            let path = directory.path().join(name);
            std::fs::write(&path, FIXTURE).unwrap();
            let metadata = probe_metadata(&path).unwrap();
            assert_eq!(metadata.duration_seconds, Some(2.0 / 48_000.0));
            assert_eq!(metadata.sample_rate, Some(48_000));
            assert_eq!(metadata.channels, Some(2));
            assert_eq!(metadata.bits_per_sample, Some(16));

            let full = decode_audio_typed(&path, None).unwrap();
            assert_eq!(full.samples, [-1.0, 0.5, 0.0, -0.5]);
            assert_eq!(full.sample_rate, 48_000);
            assert_eq!(full.channels, 2);
            let limited = decode_audio_typed(&path, Some(0.5 / 48_000.0)).unwrap();
            assert_eq!(limited.samples, [-1.0, 0.5]);
        }
    }

    #[test]
    fn invalid_aiff_metadata_fails_before_decoding() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.aiff");
        let mut zero_rate = FIXTURE.to_vec();
        zero_rate[28..38].fill(0);
        std::fs::write(&path, zero_rate).unwrap();
        assert!(probe_metadata(&path).unwrap_err().contains("sample rate"));
        assert!(matches!(
            decode_audio_typed(&path, None),
            Err(AnalysisDecodeError::Invalid(_))
        ));

        std::fs::write(&path, &FIXTURE[..FIXTURE.len() - 1]).unwrap();
        assert!(probe_metadata(&path).unwrap_err().contains("sample data"));
        assert!(matches!(
            decode_audio_typed(&path, None),
            Err(AnalysisDecodeError::Corrupt(_))
        ));

        let mut twelve_bit = FIXTURE.to_vec();
        twelve_bit[26..28].copy_from_slice(&12_i16.to_be_bytes());
        std::fs::write(&path, twelve_bit).unwrap();
        assert!(
            probe_metadata(&path)
                .unwrap_err()
                .contains("uncompressed AIFF PCM")
        );
        assert!(matches!(
            decode_audio_typed(&path, None),
            Err(AnalysisDecodeError::Unsupported(_))
        ));
    }

    #[test]
    fn aiff_probe_bounds_header_reads_across_many_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-chunks.aif");
        let mut bytes = FIXTURE[..12].to_vec();
        for _ in 0..=(MAX_HEADER_READ_BYTES / 8) {
            bytes.extend_from_slice(b"JUNK\0\0\0\0");
        }
        bytes.extend_from_slice(&FIXTURE[12..]);
        let form_len = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&form_len.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(probe_metadata(&path).unwrap_err().contains("read budget"));
    }
}
