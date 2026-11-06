//! Bounded uncompressed AIFF PCM reading and exact frame seek.

use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use aifc::{AifcError, AifcReader, FileFormat, Sample};

/// Structured failure opening or reading AIFF audio.
#[derive(Debug)]
pub enum AiffReadError {
    /// The container or PCM width is outside the supported format.
    Unsupported(String),
    /// The container is truncated, inconsistent, or unreadable.
    Corrupt(String),
    /// A required value or requested frame is invalid.
    Invalid(String),
    /// A filesystem operation failed.
    Io {
        /// Error category from the filesystem operation.
        kind: io::ErrorKind,
        /// Original diagnostic with source context.
        detail: String,
    },
}

impl fmt::Display for AiffReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(detail)
            | Self::Corrupt(detail)
            | Self::Invalid(detail)
            | Self::Io { detail, .. } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for AiffReadError {}

const MAX_HEADER_READ_BYTES: usize = 64 * 1024;

/// Match the two supported AIFF filename extensions, ignoring ASCII case.
pub fn is_aiff_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("aif") || extension.eq_ignore_ascii_case("aiff")
        })
}

/// Validated, seekable reader for uncompressed AIFF PCM.
pub struct AiffPcmReader {
    reader: AifcReader<HeaderBudget>,
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u32,
    frames: u64,
}

impl AiffPcmReader {
    /// Open a path and validate its AIFF metadata and complete sound frames.
    pub fn open(path: &Path) -> Result<Self, AiffReadError> {
        let file = File::open(path).map_err(|error| io_error(path, error))?;
        Self::from_file(file, path)
    }

    /// Validate an already opened source descriptor positioned at the start of the AIFF file.
    ///
    /// The path is used for diagnostics. This method only reads from the supplied file.
    pub fn from_file(file: File, path: &Path) -> Result<Self, AiffReadError> {
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
            return Err(AiffReadError::Unsupported(format!(
                "{} requires 8-, 16-, 24-, or 32-bit uncompressed AIFF PCM",
                path.display()
            )));
        }
        if !(1.0..=f64::from(u32::MAX)).contains(&info.sample_rate)
            || info.sample_rate.fract() != 0.0
        {
            return Err(AiffReadError::Invalid(format!(
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
            return Err(AiffReadError::Corrupt(format!(
                "{} has missing, incomplete, or inconsistent AIFF sample data",
                path.display()
            )));
        }

        Ok(Self {
            reader,
            path: path.to_path_buf(),
            sample_rate: info.sample_rate as u32,
            channels,
            bits_per_sample,
            frames,
        })
    }

    /// Return the integer sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Return the number of interleaved channels.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Return the number of complete frames in the sound chunk.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Seek to the first channel of an exact frame. The end frame positions the reader at EOF.
    pub fn seek_frame(&mut self, frame: u64) -> Result<(), AiffReadError> {
        if frame > self.frames {
            return Err(AiffReadError::Invalid(format!(
                "{} AIFF seek frame {frame} exceeds frame count {}",
                self.path.display(),
                self.frames
            )));
        }
        self.reader
            .seek(frame * u64::from(self.channels))
            .map_err(|error| read_error(&self.path, error))
    }

    /// Return the number of significant bits per PCM sample.
    pub fn bits_per_sample(&self) -> u32 {
        self.bits_per_sample
    }

    /// Read the next normalized, interleaved sample.
    pub fn read_sample(&mut self) -> Result<Option<f32>, AiffReadError> {
        let sample = self
            .reader
            .read_sample()
            .map_err(|error| read_error(&self.path, error))?;
        let value = match sample {
            Some(Sample::I8(value)) => f32::from(value) / 128.0,
            Some(Sample::I16(value)) => f32::from(value) / 32_768.0,
            Some(Sample::I24(value)) => value as f32 / 8_388_608.0,
            Some(Sample::I32(value)) => value as f32 / 2_147_483_648.0,
            None => return Ok(None),
            _ => {
                return Err(AiffReadError::Corrupt(format!(
                    "{} contains unexpected AIFF sample data",
                    self.path.display()
                )));
            }
        };
        Ok(Some(value))
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

fn read_error(path: &Path, error: AifcError) -> AiffReadError {
    let detail = format!("AIFF read failed for {}: {error:?}", path.display());
    match error {
        AifcError::Unsupported | AifcError::UnrecognizedFormat => {
            AiffReadError::Unsupported(detail)
        }
        AifcError::StdIoError(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            io_error(path, error)
        }
        _ => AiffReadError::Corrupt(detail),
    }
}

fn io_error(path: &Path, error: io::Error) -> AiffReadError {
    AiffReadError::Io {
        kind: error.kind(),
        detail: format!("AIFF I/O failed for {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/stereo-pcm16.aiff"
    ));

    #[test]
    fn seek_frame_aligns_to_the_first_channel_and_supports_eof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stereo.aiff");
        std::fs::write(&path, STEREO_FIXTURE).unwrap();
        let mut reader = AiffPcmReader::open(&path).unwrap();

        reader.seek_frame(1).unwrap();
        assert_eq!(reader.read_sample().unwrap(), Some(0.0));
        assert_eq!(reader.read_sample().unwrap(), Some(-0.5));
        assert_eq!(reader.read_sample().unwrap(), None);

        reader.seek_frame(reader.frames()).unwrap();
        assert_eq!(reader.read_sample().unwrap(), None);
        assert!(matches!(
            reader.seek_frame(3),
            Err(AiffReadError::Invalid(_))
        ));

        reader.seek_frame(0).unwrap();
        assert_eq!(reader.read_sample().unwrap(), Some(-1.0));
    }

    #[test]
    fn aiff_probe_bounds_header_reads_across_many_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-chunks.aif");
        let mut bytes = STEREO_FIXTURE[..12].to_vec();
        for _ in 0..=(MAX_HEADER_READ_BYTES / 8) {
            bytes.extend_from_slice(b"JUNK\0\0\0\0");
        }
        bytes.extend_from_slice(&STEREO_FIXTURE[12..]);
        let form_len = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&form_len.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(
            AiffPcmReader::open(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("read budget")
        );
    }
}
