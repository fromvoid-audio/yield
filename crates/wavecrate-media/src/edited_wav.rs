//! Float WAV encoding for caller-owned edited audio.

use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;

/// Encode complete interleaved frames using the native editor's 32-bit float WAV format.
///
/// Sample bits, including values outside full scale, signed zero and nonfinite values, are
/// preserved. The sink must be empty and positioned at zero. This only encodes bytes: it does
/// not publish, synchronize, replace or own a destination. Discard the sink on any error.
pub fn encode_edited_wav<W: Write + Seek>(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    sink: &mut W,
) -> Result<(), String> {
    encode_edited_wav_with_cancel(samples, channels, sample_rate, sink, None)
}

/// Encode edited audio with bounded additional memory and cooperative cancellation.
///
/// Cancellation is checked before writing, every 4,096 samples and around finalization.
/// Any error, including cancellation after finalization, requires discarding the entire sink.
/// See [`encode_edited_wav`] for sample and destination semantics.
pub fn encode_edited_wav_with_cancel<W: Write + Seek>(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    sink: &mut W,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    check_cancellation(cancellation)?;
    let spec = checked_spec(samples.len(), channels, sample_rate)?;
    let position = sink
        .stream_position()
        .map_err(|error| format!("failed to inspect edited WAV sink: {error}"))?;
    if position != 0 {
        return Err(String::from("edited WAV sink must be positioned at zero"));
    }
    let length = sink
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("failed to inspect edited WAV sink length: {error}"))?;
    sink.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind edited WAV sink: {error}"))?;
    if length != 0 {
        return Err(String::from("edited WAV sink must be empty"));
    }
    check_cancellation(cancellation)?;
    let mut writer = hound::WavWriter::new(sink, spec)
        .map_err(|error| format!("failed to write edited WAV header: {error}"))?;
    for block in samples.chunks(4_096) {
        check_cancellation(cancellation)?;
        for sample in block {
            writer
                .write_sample(*sample)
                .map_err(|error| format!("failed to write edited WAV sample: {error}"))?;
        }
    }
    check_cancellation(cancellation)?;
    writer
        .finalize()
        .map_err(|error| format!("failed to finalize edited WAV: {error}"))?;
    check_cancellation(cancellation)
}

fn check_cancellation(cancellation: Option<&AtomicBool>) -> Result<(), String> {
    if crate::is_cancelled(cancellation) {
        Err(String::from("edited WAV encoding canceled"))
    } else {
        Ok(())
    }
}

fn checked_spec(
    sample_count: usize,
    channels: usize,
    sample_rate: u32,
) -> Result<hound::WavSpec, String> {
    let channels = u16::try_from(channels)
        .ok()
        .filter(|channels| *channels > 0)
        .ok_or_else(|| String::from("invalid edited WAV channel count"))?;
    let block_align = channels
        .checked_mul(4)
        .ok_or_else(|| String::from("edited WAV block alignment overflows"))?;
    if sample_rate == 0 || sample_rate.checked_mul(u32::from(block_align)).is_none() {
        return Err(String::from("invalid edited WAV sample rate or byte rate"));
    }
    if sample_count == 0 || sample_count % usize::from(channels) != 0 {
        return Err(String::from("edited WAV requires nonempty complete frames"));
    }
    // Hound emits WAVEFORMATEXTENSIBLE for 32-bit samples: RIFF payload is 60 + data bytes.
    u64::try_from(sample_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(60))
        .filter(|size| *size <= u64::from(u32::MAX))
        .ok_or_else(|| String::from("edited WAV exceeds RIFF size limits"))?;
    Ok(hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};
    use std::sync::atomic::Ordering;

    #[test]
    fn preserves_stereo_float_bits_and_native_format() {
        let samples = [
            2.5,
            -3.0,
            -0.0,
            f32::from_bits(0x7fc01234),
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let mut output = Cursor::new(Vec::new());
        encode_edited_wav(&samples, 2, 48_000, &mut output).unwrap();
        let bytes = output.into_inner();
        assert_eq!(bytes.len(), 68 + samples.len() * 4);
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(
            reader.spec(),
            checked_spec(samples.len(), 2, 48_000).unwrap()
        );
        let actual: Vec<u32> = reader
            .samples::<f32>()
            .map(|sample| sample.unwrap().to_bits())
            .collect();
        assert_eq!(actual, samples.map(f32::to_bits));
    }

    #[test]
    fn transformed_stereo_audio_round_trips_including_outer_fades() {
        use wavecrate_core::{WaveformEditFade, WaveformEditSettings};

        let mut samples = vec![0.75_f32; 64];
        let settings = WaveformEditSettings::new(
            2.0,
            Some(WaveformEditFade::new(0.0, 0.0, 0.5, 0.25).unwrap()),
            Some(WaveformEditFade::new(0.0, 0.0, 0.5, 0.5).unwrap()),
        )
        .unwrap();
        crate::apply_waveform_edit_effects(&mut samples, 2, 1_000, 8, 24, settings).unwrap();
        assert!(samples[..16].iter().any(|sample| *sample != 0.75));
        assert!(samples[48..].iter().any(|sample| *sample != 0.75));
        assert!(samples.iter().any(|sample| *sample > 1.0));
        let mut output = Cursor::new(Vec::new());
        encode_edited_wav(&samples, 2, 1_000, &mut output).unwrap();
        let mut reader = hound::WavReader::new(Cursor::new(output.into_inner())).unwrap();
        let actual: Vec<u32> = reader
            .samples::<f32>()
            .map(|sample| sample.unwrap().to_bits())
            .collect();
        assert_eq!(
            actual,
            samples
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn invalid_layouts_leave_sink_unwritten() {
        for (samples, channels, rate) in [
            (vec![1.0], 0, 48_000),
            (vec![1.0], 65_536, 48_000),
            (vec![1.0], 16_384, 48_000),
            (vec![1.0], 1, 0),
            (vec![1.0], 1, u32::MAX),
            (vec![], 1, 48_000),
            (vec![1.0], 2, 48_000),
        ] {
            let mut output = Cursor::new(Vec::new());
            assert!(encode_edited_wav(&samples, channels, rate, &mut output).is_err());
            assert!(output.into_inner().is_empty());
        }
        assert!(checked_spec(usize::MAX, 1, 48_000).is_err());
        let max_count = ((u64::from(u32::MAX) - 60) / 4) as usize;
        assert!(checked_spec(max_count, 1, 48_000).is_ok());
        assert!(checked_spec(max_count + 1, 1, 48_000).is_err());
    }

    #[test]
    fn rejects_existing_contents_and_nonzero_position_without_overwrite() {
        for position in [0, 2] {
            let mut output = Cursor::new(vec![1, 2, 3]);
            output.set_position(position);
            assert!(encode_edited_wav(&[0.0], 1, 48_000, &mut output).is_err());
            assert_eq!(output.into_inner(), [1, 2, 3]);
        }
    }

    struct TestSink<'a> {
        output: Cursor<Vec<u8>>,
        cancel: &'a AtomicBool,
        cancel_after: usize,
        fail_write: bool,
        fail_seek: bool,
        fail_finalize_seek: bool,
        fail_flush: bool,
        cancel_on_flush: bool,
    }

    impl<'a> TestSink<'a> {
        fn new(cancel: &'a AtomicBool) -> Self {
            Self {
                output: Cursor::new(Vec::new()),
                cancel,
                cancel_after: usize::MAX,
                fail_write: false,
                fail_seek: false,
                fail_finalize_seek: false,
                fail_flush: false,
                cancel_on_flush: false,
            }
        }
    }

    impl Write for TestSink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::other("write fixture"));
            }
            let written = self.output.write(bytes)?;
            if self.output.get_ref().len() >= self.cancel_after {
                self.cancel.store(true, Ordering::Relaxed);
            }
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.cancel_on_flush {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if self.fail_flush {
                Err(io::Error::other("flush fixture"))
            } else {
                Ok(())
            }
        }
    }
    impl Seek for TestSink<'_> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if self.fail_seek || (self.fail_finalize_seek && !self.output.get_ref().is_empty()) {
                Err(io::Error::other("seek fixture"))
            } else {
                self.output.seek(position)
            }
        }
    }

    #[test]
    fn cancellation_before_during_and_after_encoding_is_reported() {
        let cancel = AtomicBool::new(true);
        let mut output = TestSink::new(&cancel);
        assert!(
            encode_edited_wav_with_cancel(&[0.0], 1, 48_000, &mut output, Some(&cancel))
                .unwrap_err()
                .contains("canceled")
        );
        assert!(output.output.get_ref().is_empty());
        cancel.store(false, Ordering::Relaxed);
        output.cancel_after = 100;
        assert!(
            encode_edited_wav_with_cancel(
                &vec![0.0; 20_000],
                1,
                48_000,
                &mut output,
                Some(&cancel)
            )
            .unwrap_err()
            .contains("canceled")
        );
        assert!(output.output.get_ref().len() <= 68 + 4096 * 4);
        cancel.store(false, Ordering::Relaxed);
        let mut output = TestSink::new(&cancel);
        output.cancel_on_flush = true;
        assert!(
            encode_edited_wav_with_cancel(&[0.0], 1, 48_000, &mut output, Some(&cancel))
                .unwrap_err()
                .contains("canceled")
        );
    }

    #[test]
    fn propagates_sink_write_seek_and_finalize_failures() {
        let cancel = AtomicBool::new(false);
        for failure in 0..4 {
            let mut output = TestSink::new(&cancel);
            output.fail_write = failure == 0;
            output.fail_seek = failure == 1;
            output.fail_finalize_seek = failure == 2;
            output.fail_flush = failure == 3;
            assert!(encode_edited_wav(&[0.5], 1, 48_000, &mut output).is_err());
        }
    }
}
