use std::{io::Read, sync::atomic::AtomicBool};

use super::{
    MAX_TRANSIENT_MARKERS, TransientMarkers, TransientPreset, check_cancelled, detect_transients,
    marker_gap,
};
use crate::analysis::audio::wav_pcm::{WavPcm, open_wav, read_pcm_samples};

/// Detector/config identity for markers computed in overlapping WAV chunks.
pub const STREAM_DETECTOR_VERSION: &str = "native-windowed-spectral-flux-v1";
const MAX_ENCODED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STREAM_SAMPLES: usize = 32_000_000;
const CORE_FRAMES: usize = 32_768;
// Covers the largest FFT window and the detector's four prior hops.
const CONTEXT_FRAMES: usize = 2_048;

/// Analyze native frames from a forward-only WAV input without retaining the whole recording.
///
/// Admits up to 32 million interleaved samples within a 256 MiB encoded read limit.
/// Each core range owns its markers, with the surrounding PCM used only as detector context.
/// The caller owns content verification and should keep the result private until it succeeds.
pub fn detect_streaming_transients(
    input: impl Read,
    preset: TransientPreset,
    cancel: &AtomicBool,
) -> Result<TransientMarkers, &'static str> {
    check_cancelled(cancel)?;
    let mut reader = open_wav(input.take(MAX_ENCODED_BYTES))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    let samples = reader.len() as usize;
    if samples == 0 || samples > MAX_STREAM_SAMPLES {
        return Err("WAV streaming sample limit");
    }
    let frames = samples / channels;
    let min_gap = marker_gap(spec.sample_rate, frames);
    let mut markers = Vec::new();
    let mut buffer = Vec::with_capacity(frames.min(CORE_FRAMES + 2 * CONTEXT_FRAMES) * channels);
    let mut buffered_start = 0;
    let mut read_end = 0;
    for core_start in (0..frames).step_by(CORE_FRAMES) {
        check_cancelled(cancel)?;
        let core_end = (core_start + CORE_FRAMES).min(frames);
        let window_start = core_start.saturating_sub(CONTEXT_FRAMES);
        let window_end = (core_end + CONTEXT_FRAMES).min(frames);
        buffer.drain(..(window_start - buffered_start) * channels);
        read_pcm_samples(
            &mut reader,
            &mut buffer,
            (window_end - read_end) * channels,
            cancel,
        )?;
        buffered_start = window_start;
        read_end = window_end;

        if buffer.iter().all(|&sample| sample == 0.0) {
            continue;
        }
        let pcm = WavPcm::from_complete_frames(buffer, spec);
        for &frame in detect_transients(&pcm, preset, cancel)?.frames() {
            let absolute = window_start + frame as usize;
            if (core_start..core_end).contains(&absolute)
                && markers
                    .last()
                    .is_none_or(|&previous| absolute >= previous as usize + min_gap)
            {
                markers.push(absolute as u32);
            }
        }
        buffer = pcm.into_samples();
    }
    check_cancelled(cancel)?;
    debug_assert!(markers.len() <= MAX_TRANSIENT_MARKERS);
    TransientMarkers::from_ordered_frames(frames as u32, preset, markers)
        .ok_or("Invalid streaming transient result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn stereo_wav(total_frames: usize, attacks: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer = hound::WavWriter::new(
            Cursor::new(&mut bytes),
            hound::WavSpec {
                channels: 2,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..total_frames {
            let sample: i16 = if attacks.contains(&frame) { 24_000 } else { 0 };
            writer.write_sample(sample).unwrap();
            writer.write_sample(-sample).unwrap();
        }
        writer.finalize().unwrap();
        bytes
    }

    #[test]
    fn streaming_transients_keep_antiphase_attacks_exact_across_chunks() {
        let attacks = [CORE_FRAMES - 1, 2 * CORE_FRAMES, 3 * CORE_FRAMES + 1];
        let length = 3 * CORE_FRAMES + 512;
        let bytes = stereo_wav(length, &attacks);
        let result = detect_streaming_transients(
            bytes.as_slice(),
            TransientPreset::Normal,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.source_frames(), length as u32);
        assert_eq!(result.frames(), attacks.map(|frame| frame as u32));
    }

    #[test]
    fn streaming_transients_reject_cancelled_truncated_and_oversized_pcm() {
        let cancel = AtomicBool::new(false);
        let mut bytes = stereo_wav(4096, &[200]);
        assert_eq!(
            detect_streaming_transients(
                bytes.as_slice(),
                TransientPreset::Normal,
                &AtomicBool::new(true)
            )
            .unwrap_err(),
            "Cancelled"
        );
        bytes.pop();
        assert!(
            detect_streaming_transients(bytes.as_slice(), TransientPreset::Normal, &cancel)
                .is_err()
        );
        let data_bytes = ((MAX_STREAM_SAMPLES + 2) * 2) as u32;
        bytes[40..44].copy_from_slice(&data_bytes.to_le_bytes());
        assert_eq!(
            detect_streaming_transients(bytes.as_slice(), TransientPreset::Normal, &cancel)
                .unwrap_err(),
            "WAV streaming sample limit"
        );
    }

    #[test]
    fn streaming_transients_stop_reading_when_cancelled_after_the_first_chunk() {
        struct CancelDuringRead<'a> {
            remaining: &'a [u8],
            cancel: &'a AtomicBool,
            until_cancel: usize,
        }
        impl Read for CancelDuringRead<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let read = self.remaining.read(output)?;
                self.until_cancel = self.until_cancel.saturating_sub(read);
                if self.until_cancel == 0 {
                    self.cancel
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                Ok(read)
            }
        }

        let bytes = stereo_wav(3 * CORE_FRAMES, &[]);
        let cancel = AtomicBool::new(false);
        let mut reader = CancelDuringRead {
            remaining: &bytes,
            cancel: &cancel,
            until_cancel: (CORE_FRAMES + CONTEXT_FRAMES + 1024) * 4,
        };
        assert_eq!(
            detect_streaming_transients(&mut reader, TransientPreset::Normal, &cancel).unwrap_err(),
            "Cancelled",
        );
        assert!(reader.remaining.len() > CORE_FRAMES * 4);
    }
}
