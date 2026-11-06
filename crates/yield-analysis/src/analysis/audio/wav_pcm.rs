use std::{
    io::{Cursor, Read},
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_WAV_BYTES: usize = 8 * 1024 * 1024;
const MAX_PCM_SAMPLES: usize = 2_000_000;

/// Rejection from the whole-buffer native WAV decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WavPcmDecodeError {
    /// The encoded input exceeds the whole-buffer byte limit.
    #[error("WAV input byte limit")]
    ByteLimit,
    /// The decoded input exceeds the whole-buffer sample limit.
    #[error("WAV sample limit")]
    SampleLimit,
    /// The input was invalid, incomplete, unsupported or interrupted.
    #[error("{0}")]
    Input(&'static str),
}

impl WavPcmDecodeError {
    /// Bounded diagnostic for the current WAV contract.
    pub fn message(self) -> &'static str {
        match self {
            Self::ByteLimit => "WAV input byte limit",
            Self::SampleLimit => "WAV sample limit",
            Self::Input(reason) => reason,
        }
    }
}

impl From<&'static str> for WavPcmDecodeError {
    fn from(reason: &'static str) -> Self {
        Self::Input(reason)
    }
}

/// Complete, interleaved WAV frames in their original channel order and sample rate.
#[derive(Debug)]
pub struct WavPcm {
    samples: Vec<f32>,
    channels: u16,
    sample_rate: u32,
}

impl WavPcm {
    pub(crate) fn from_complete_frames(samples: Vec<f32>, spec: hound::WavSpec) -> Self {
        debug_assert!(samples.len().is_multiple_of(usize::from(spec.channels)));
        Self {
            samples,
            channels: spec.channels,
            sample_rate: spec.sample_rate,
        }
    }

    pub(crate) fn into_samples(self) -> Vec<f32> {
        self.samples
    }

    /// Interleaved PCM. Integer input is scaled to full scale and float input is preserved.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Number of samples per source frame.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Original WAV sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Decode captured WAV bytes without downmixing, trimming or resampling.
///
/// Admits at most 8 MiB and 2,000,000 interleaved samples. An input beyond
/// either limit fails instead of returning a partial recording.
pub fn decode_wav_pcm(bytes: &[u8], cancel: &AtomicBool) -> Result<WavPcm, WavPcmDecodeError> {
    decode_pcm(bytes, None, cancel)
}

pub(super) fn decode_pcm(
    bytes: &[u8],
    prefix_seconds: Option<usize>,
    cancel: &AtomicBool,
) -> Result<WavPcm, WavPcmDecodeError> {
    if cancel.load(Ordering::Acquire) {
        return Err("Cancelled".into());
    }
    if bytes.len() > MAX_WAV_BYTES {
        return Err(WavPcmDecodeError::ByteLimit);
    }
    let mut reader = open_wav(Cursor::new(bytes))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    // Hound validates that the declared data chunk contains complete frames.
    let source_samples = reader.len() as usize;
    let limit = match prefix_seconds {
        Some(seconds) => source_samples.min(
            (spec.sample_rate as usize * channels)
                .checked_mul(seconds)
                .ok_or("WAV sample limit")?,
        ),
        None => source_samples,
    };
    if limit == 0 {
        return Err("WAV input contains no samples".into());
    }
    if limit > MAX_PCM_SAMPLES {
        return Err(WavPcmDecodeError::SampleLimit);
    }
    let mut samples = Vec::with_capacity(limit);
    read_pcm_samples(&mut reader, &mut samples, limit, cancel)?;
    Ok(WavPcm::from_complete_frames(samples, spec))
}

pub(crate) fn open_wav<R: Read>(input: R) -> Result<hound::WavReader<R>, &'static str> {
    let reader = hound::WavReader::new(input).map_err(|_| "Invalid WAV input")?;
    let spec = reader.spec();
    if spec.channels == 0
        || spec.channels > 8
        || spec.sample_rate == 0
        || spec.sample_rate > 192_000
        || spec.bits_per_sample == 0
        || spec.bits_per_sample > 32
        || (spec.sample_format == hound::SampleFormat::Float && spec.bits_per_sample != 32)
    {
        return Err("Unsupported WAV parameters");
    }
    Ok(reader)
}

pub(crate) fn read_pcm_samples<R: Read>(
    reader: &mut hound::WavReader<R>,
    samples: &mut Vec<f32>,
    count: usize,
    cancel: &AtomicBool,
) -> Result<(), &'static str> {
    let spec = reader.spec();
    let expected_len = samples.len() + count;
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>().take(count) {
                if cancel.load(Ordering::Acquire) {
                    return Err("Cancelled");
                }
                samples.push(sample.map_err(|_| "Invalid WAV sample")?);
            }
        }
        hound::SampleFormat::Int => {
            let scale = 2.0f32.powi(i32::from(spec.bits_per_sample) - 1);
            for sample in reader.samples::<i32>().take(count) {
                if cancel.load(Ordering::Acquire) {
                    return Err("Cancelled");
                }
                samples.push(sample.map_err(|_| "Invalid WAV sample")? as f32 / scale);
            }
        }
    }
    if samples.len() != expected_len {
        return Err("Incomplete WAV frame");
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer = hound::WavWriter::new(
            Cursor::new(&mut bytes),
            hound::WavSpec {
                channels,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .expect("create fixture WAV");
        for sample in samples {
            writer.write_sample(*sample).expect("write fixture frame");
        }
        writer.finalize().expect("finish fixture WAV");
        bytes
    }

    #[test]
    fn full_wav_preserves_native_mono_and_stereo_frames() {
        let cancel = AtomicBool::new(false);
        let mono = pcm_wav(1, 44_100, &[0, 16_384, -32_768]);
        let pcm = decode_wav_pcm(&mono, &cancel).expect("mono PCM");
        assert_eq!(pcm.sample_rate(), 44_100);
        assert_eq!(pcm.channels(), 1);
        assert_eq!(pcm.samples(), [0.0, 0.5, -1.0]);

        let stereo = pcm_wav(2, 48_000, &[0, 0, -16_384, 16_384, 8192, 0]);
        let pcm = decode_wav_pcm(&stereo, &cancel).expect("stereo PCM");
        assert_eq!(pcm.sample_rate(), 48_000);
        assert_eq!(pcm.channels(), 2);
        assert_eq!(pcm.samples(), [0.0, 0.0, -0.5, 0.5, 0.25, 0.0]);
    }

    #[test]
    fn rejects_truncated_malformed_and_cancelled_wav() {
        let cancel = AtomicBool::new(false);
        let mut stereo = pcm_wav(2, 48_000, &[0, 0, -16_384, 16_384]);
        assert!(decode_wav_pcm(b"not a WAV", &cancel).is_err());
        assert_eq!(
            decode_wav_pcm(&stereo, &AtomicBool::new(true)).unwrap_err(),
            WavPcmDecodeError::Input("Cancelled")
        );
        stereo.pop();
        assert!(decode_wav_pcm(&stereo, &cancel).is_err());
    }

    #[test]
    fn full_decode_rejects_oversize_instead_of_returning_a_prefix() {
        let cancel = AtomicBool::new(false);
        let mut wav = pcm_wav(1, 48_000, &[0; 4]);
        let oversized_data_len = ((MAX_PCM_SAMPLES + 1) * 2) as u32;
        wav[40..44].copy_from_slice(&oversized_data_len.to_le_bytes());
        assert_eq!(
            decode_wav_pcm(&wav, &cancel).unwrap_err(),
            WavPcmDecodeError::SampleLimit
        );
        wav.resize(MAX_WAV_BYTES + 1, 0);
        assert_eq!(
            decode_wav_pcm(&wav, &cancel).unwrap_err(),
            WavPcmDecodeError::ByteLimit
        );
    }

    #[test]
    fn feature_prefix_stops_at_a_whole_native_frame() {
        let mut frames = vec![0; 2 * 8 * 1000];
        frames.extend([16_384, -16_384]);
        let stereo = pcm_wav(2, 1000, &frames);
        let cancel = AtomicBool::new(false);
        let prefix = decode_pcm(&stereo, Some(8), &cancel).expect("bounded prefix");
        let full = decode_wav_pcm(&stereo, &cancel).expect("full PCM");
        assert_eq!(prefix.samples().len(), frames.len() - 2);
        assert_eq!(full.samples().len(), frames.len());
        assert_eq!(full.samples()[prefix.samples().len()..], [0.5, -0.5]);
    }
}
