use std::path::Path;

use yield_aiff::{AiffPcmReader, AiffReadError};

use super::audio::AudioProbe;
use super::audio_decode::{AnalysisDecodeError, DecodedAudio};

pub(super) fn probe(path: &Path) -> Result<AudioProbe, AnalysisDecodeError> {
    let audio = AiffPcmReader::open(path)?;
    Ok(AudioProbe {
        duration_seconds: Some((audio.frames() as f64 / f64::from(audio.sample_rate())) as f32),
        sample_rate: Some(audio.sample_rate()),
        channels: Some(audio.channels()),
        bits_per_sample: Some(audio.bits_per_sample()),
    })
}

pub(super) fn decode(
    path: &Path,
    max_seconds: Option<f32>,
) -> Result<DecodedAudio, AnalysisDecodeError> {
    let mut audio = AiffPcmReader::open(path)?;
    let frames = max_seconds
        .filter(|limit| *limit > 0.0)
        .map(|limit| {
            let limited = (f64::from(limit) * f64::from(audio.sample_rate())).ceil() as u64;
            limited.min(audio.frames())
        })
        .unwrap_or(audio.frames());
    let mut samples = Vec::new();
    for _ in 0..frames * u64::from(audio.channels()) {
        let value = audio.read_sample()?.ok_or_else(|| {
            AnalysisDecodeError::Corrupt(format!("{} ended during AIFF decode", path.display()))
        })?;
        samples.push(value);
    }
    Ok(DecodedAudio {
        samples,
        sample_rate: audio.sample_rate(),
        channels: audio.channels(),
    })
}

impl From<AiffReadError> for AnalysisDecodeError {
    fn from(error: AiffReadError) -> Self {
        match error {
            AiffReadError::Unsupported(detail) => Self::Unsupported(detail),
            AiffReadError::Corrupt(detail) => Self::Corrupt(detail),
            AiffReadError::Invalid(detail) => Self::Invalid(detail),
            AiffReadError::Io { kind, detail } => Self::Io { kind, detail },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{audio::probe_metadata, audio_decode::decode_audio_typed};

    // Two stereo frames with signed 16-bit big-endian PCM at 48 kHz.
    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/stereo-pcm16.aiff"
    ));

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
}
