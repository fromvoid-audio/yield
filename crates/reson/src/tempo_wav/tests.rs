use super::*;
use crate::{TempoAuditionTarget, prepare_tempo_audition};
use hound::{WavSpec, WavSpecEx, WavWriter};
use std::{sync::atomic::Ordering, time::Duration};

fn deadline() -> Instant {
    Instant::now() + Duration::from_millis(500)
}

fn integer_wav(bits: u16, sample_bytes: u16, rate: u32, frames: usize) -> Cursor<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    let spec = WavSpecEx {
        spec: WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        },
        bytes_per_sample: sample_bytes,
    };
    let mut writer = WavWriter::new_with_spec_ex(&mut bytes, spec).unwrap();
    let quarter = 1 << (bits - 3);
    for frame in 0..frames {
        let sample = if frame % 2 == 0 { quarter } else { -quarter };
        writer.write_sample(sample).unwrap();
        writer.write_sample(-sample / 2).unwrap();
    }
    writer.finalize().unwrap();
    bytes
}

#[test]
fn tempo_wav_reads_exact_interleaved_integer_span_at_nonzero_offset() {
    for (bits, bytes) in [(8, 1), (16, 2), (24, 3), (32, 4)] {
        let mut wav = integer_wav(bits, bytes, 48_000, 10);
        let span =
            read_tempo_wav_span(&mut wav, 3..6, &AtomicBool::new(false), deadline()).unwrap();
        assert_eq!(span.source_span, 3..6);
        assert_eq!(span.source_frames, 10);
        assert_eq!((span.sample_rate, span.channels), (48_000, 2));
        assert_eq!(span.samples, [-0.25, 0.125, 0.25, -0.125, -0.25, 0.125]);
        let riff = wav.get_ref();
        let header = hound::WavReader::new(Cursor::new(riff)).unwrap();
        let data_start = header.into_inner().position();
        assert_eq!(wav.position(), data_start + 6 * 2 * u64::from(bytes));
    }
}

#[test]
fn tempo_wav_float_span_flows_into_explicit_target_preparation() {
    let mut bytes = Cursor::new(Vec::new());
    let spec = WavSpec {
        channels: 2,
        sample_rate: 12_000,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::new(&mut bytes, spec).unwrap();
    for frame in 0..1_300 {
        let left = if (75..1_276).contains(&frame) {
            0.25
        } else {
            0.75
        };
        writer.write_sample(left as f32).unwrap();
        writer.write_sample(-0.125_f32).unwrap();
    }
    writer.finalize().unwrap();

    let cancel = AtomicBool::new(false);
    let expires = deadline();
    let span = read_tempo_wav_span(&mut bytes, 75..1_276, &cancel, expires).unwrap();
    let prepared = prepare_tempo_audition(
        TempoAuditionTarget::Enabled {
            source_bpm: Some(100.0),
            target_bpm: Some(150.0),
        },
        span.source_span,
        &span.samples,
        span.sample_rate,
        span.channels,
        &cancel,
        expires,
    )
    .unwrap();
    assert_eq!(prepared.frames.source_span(), 75..1_276);
    assert_eq!(prepared.frames.output_frames(), 801);
    for frame in prepared.samples.chunks_exact(2) {
        assert!((frame[0] - 0.25).abs() < 1e-6);
        assert!((frame[1] + 0.125).abs() < 1e-6);
    }
}

#[test]
fn tempo_wav_rejects_padded_inconsistent_truncated_and_oversized_header() {
    let cancel = AtomicBool::new(false);
    let mut padded = integer_wav(24, 4, 48_000, 4);
    assert!(matches!(
        read_tempo_wav_span(&mut padded, 1..2, &cancel, deadline()),
        Err(TempoWavReadError::InvalidWav(_))
    ));

    let mut malformed = integer_wav(16, 2, 48_000, 4);
    // Advertise an invalid five-byte stereo frame that Hound would divide down to two bytes per sample.
    let bytes = malformed.get_mut();
    bytes[28..32].copy_from_slice(&(48_000_u32 * 5).to_le_bytes());
    bytes[32..34].copy_from_slice(&5_u16.to_le_bytes());
    assert!(matches!(
        read_tempo_wav_span(&mut malformed, 1..2, &cancel, deadline()),
        Err(TempoWavReadError::InvalidWav(_))
    ));

    let mut truncated = integer_wav(16, 2, 48_000, 4);
    truncated.get_mut().pop();
    assert!(matches!(
        read_tempo_wav_span(&mut truncated, 0..1, &cancel, deadline()),
        Err(TempoWavReadError::InvalidWav(_))
    ));

    let wav = integer_wav(16, 2, 48_000, 4).into_inner();
    let mut too_far = wav[..12].to_vec();
    too_far.extend_from_slice(b"JUNK");
    too_far.extend_from_slice(&(MAX_WAV_HEADER_BYTES as u32).to_le_bytes());
    too_far.resize(too_far.len() + MAX_WAV_HEADER_BYTES as usize, 0);
    too_far.extend_from_slice(&wav[12..]);
    let riff_size = too_far.len() as u32 - 8;
    too_far[4..8].copy_from_slice(&riff_size.to_le_bytes());
    let mut source = Cursor::new(too_far);
    assert!(matches!(
        read_tempo_wav_span(&mut source, 0..1, &cancel, deadline()),
        Err(TempoWavReadError::InvalidWav(_))
    ));
    assert_eq!(source.position(), MAX_WAV_HEADER_BYTES);
}

#[test]
fn tempo_wav_checks_exact_bounds_both_budgets_and_cancellation() {
    let cancel = AtomicBool::new(false);
    let mut long = integer_wav(16, 2, 100, 802);
    assert!(matches!(
        read_tempo_wav_span(&mut long, 1..802, &cancel, deadline()),
        Err(TempoWavReadError::InputTooLarge)
    ));
    assert!(matches!(
        read_tempo_wav_span(&mut long, 1..803, &cancel, deadline()),
        Err(TempoWavReadError::InvalidSpan)
    ));
    let mut dense = integer_wav(8, 1, 192_000, 1);
    let bytes = dense.get_mut();
    let sample_count = MAX_TEMPO_INPUT_SAMPLES as u32 + 2;
    let data_start = bytes.len() - 2;
    bytes.resize(data_start + sample_count as usize, 128);
    bytes[data_start - 4..data_start].copy_from_slice(&sample_count.to_le_bytes());
    let riff_size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    assert!(matches!(
        read_tempo_wav_span(
            &mut dense,
            0..u64::from(sample_count / 2),
            &cancel,
            deadline(),
        ),
        Err(TempoWavReadError::InputTooLarge)
    ));
    assert!(matches!(
        read_tempo_wav_span(&mut long, 0..1, &cancel, Instant::now()),
        Err(TempoWavReadError::Interrupted(
            StretchPreparationError::DeadlineExceeded
        ))
    ));
    cancel.store(true, Ordering::Release);
    assert!(matches!(
        read_tempo_wav_span(&mut long, 0..1, &cancel, deadline()),
        Err(TempoWavReadError::Interrupted(
            StretchPreparationError::Canceled
        ))
    ));
}
