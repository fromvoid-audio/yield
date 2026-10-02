//! Bounded background WAV serialization into an already retained transport.
//!
//! No pathname opens or publication authority live here. The app's owner adapter remains
//! responsible for admission, retained staging identity, sync/validation and publication.
use super::health::RecordingHealthState;
use crate::input::AudioInputError;
use ringbuf::{HeapRb, traits::*};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const WRITER_DRAIN_SAMPLES: usize = 4096;
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const RUNNING: u8 = 0;
const STOPPED: u8 = 1;
const CANCELLED: u8 = 2;

/// Already retained byte transport. This interface grants no filesystem authority.
/// Yield must supply its admitted owner channel; arbitrary transports are useful for codecs/tests.
pub trait RecordingSink: Write + Seek + Send {}
impl<T: Write + Seek + Send> RecordingSink for T {}

/// Validated finite float32 WAV and callback-ring bounds.
#[derive(Clone, Copy, Debug)]
pub struct RecordingLimits {
    sample_rate: u32,
    channels: u16,
    max_frames: u64,
    max_bytes: u64,
    ring_samples: usize,
}
impl RecordingLimits {
    /// Validate before allocating a ring or writing a header to any transport.
    pub fn new(sample_rate: u32, channels: u16, max_frames: u64) -> Result<Self, AudioInputError> {
        if !(1..=192_000).contains(&sample_rate) || !(1..=8).contains(&channels) || max_frames == 0
        {
            return Err(failed("invalid finite recording format/frame bounds"));
        }
        let max_bytes = max_frames
            .checked_mul(u64::from(channels))
            .and_then(|v| v.checked_mul(4))
            .and_then(|v| v.checked_add(128))
            .filter(|v| *v <= u64::from(u32::MAX))
            .ok_or_else(|| failed("recording exceeds RIFF byte budget"))?;
        let ring_samples = usize::try_from(sample_rate)
            .ok()
            .and_then(|v| v.checked_mul(usize::from(channels)))
            .and_then(|v| v.checked_mul(2))
            .filter(|v| *v <= 3_072_000)
            .ok_or_else(|| failed("recording ring exceeds budget"))?;
        Ok(Self {
            sample_rate,
            channels,
            max_frames,
            max_bytes,
            ring_samples,
        })
    }
    /// Maximum accepted audio frames; the transport/header allowance is separately checked.
    pub fn max_frames(&self) -> u64 {
        self.max_frames
    }
    /// Conservative maximum encoded bytes including bounded header patches.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}

/// A codec completion is always unpublished; only an admitted owner can publish staging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingPrefixState {
    /// User Stop drained the accepted ring and finalized its bounded prefix.
    StoppedUnpublished,
    /// Cancellation/drop retained a bounded partial prefix; no publication/cleanup was requested.
    CancelledRetained,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RecordingStats {
    pub(super) frames: u64,
    pub(super) state: RecordingPrefixState,
}

pub(super) struct RecorderCapture {
    producer: ringbuf::HeapProd<f32>,
    health: Arc<RecordingHealthState>,
    pub(super) mode: Arc<AtomicU8>,
    channels: usize,
}
impl RecorderCapture {
    pub(super) fn submit(&mut self, samples: &[f32]) {
        if samples.len() % self.channels != 0 {
            self.health.writer_failed.store(true, Ordering::Release);
            self.mode.store(CANCELLED, Ordering::Release);
        }
        if self.mode.load(Ordering::Acquire) != RUNNING
            || self.health.writer_failed.load(Ordering::Acquire)
            || self.producer.vacant_len() < samples.len()
        {
            self.health
                .writer_dropped_samples
                .fetch_add(samples.len() as u64, Ordering::Relaxed);
            self.health
                .writer_overrun_events
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let pushed = self.producer.push_slice(samples);
        debug_assert_eq!(pushed, samples.len());
    }
}

pub(super) struct RecorderWriter {
    mode: Arc<AtomicU8>,
    join: Option<JoinHandle<Result<RecordingStats, AudioInputError>>>,
}
impl RecorderWriter {
    /// Prepare callback state without opening or writing a transport.
    pub(super) fn prepare_capture(
        limits: RecordingLimits,
        health: Arc<RecordingHealthState>,
    ) -> (RecorderCapture, ringbuf::HeapCons<f32>) {
        let (producer, consumer) = HeapRb::<f32>::new(limits.ring_samples).split();
        (
            RecorderCapture {
                producer,
                health,
                mode: Arc::new(AtomicU8::new(RUNNING)),
                channels: usize::from(limits.channels),
            },
            consumer,
        )
    }

    /// Start serialization only after paused stream construction and limits validation.
    pub(super) fn spawn_prepared(
        sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
        consumer: ringbuf::HeapCons<f32>,
        mode: Arc<AtomicU8>,
        health: Arc<RecordingHealthState>,
    ) -> Result<Self, AudioInputError> {
        let writer = WavSampleWriter::new(sink, limits)?;
        let worker_mode = Arc::clone(&mode);
        let join = thread::Builder::new()
            .name("reson-recording-writer".into())
            .spawn(move || {
                let result = writer_loop(writer, consumer, &worker_mode);
                if result.is_err() {
                    health.writer_failed.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|error| failed(&format!("recorder thread unavailable: {error}")))?;
        Ok(Self {
            mode,
            join: Some(join),
        })
    }

    #[cfg(test)]
    pub(super) fn spawn(
        sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
        health: Arc<RecordingHealthState>,
    ) -> Result<(Self, RecorderCapture), AudioInputError> {
        let (capture, consumer) = Self::prepare_capture(limits, Arc::clone(&health));
        let writer =
            Self::spawn_prepared(sink, limits, consumer, Arc::clone(&capture.mode), health)?;
        Ok((writer, capture))
    }
    /// Drain accepted frames; callers must stop/drop their input stream first.
    pub(super) fn stop(&mut self) {
        let _ = self
            .mode
            .compare_exchange(RUNNING, STOPPED, Ordering::AcqRel, Ordering::Acquire);
    }
    /// Stop at a bounded frame barrier without publishing or deleting retained output.
    pub(super) fn cancel(&mut self) {
        self.mode.store(CANCELLED, Ordering::Release);
    }
    pub(super) fn join(&mut self) -> Result<RecordingStats, AudioInputError> {
        self.join
            .take()
            .ok_or_else(|| failed("recorder writer already joined"))?
            .join()
            .map_err(|_| failed("recorder writer panicked; prefix is unconfirmed"))?
    }
}
impl Drop for RecorderWriter {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn writer_loop(
    mut writer: WavSampleWriter,
    mut consumer: ringbuf::HeapCons<f32>,
    mode: &AtomicU8,
) -> Result<RecordingStats, AudioInputError> {
    // A read batch must remain an exact number of interleaved frames for every accepted format.
    let channels = usize::from(writer.limits.channels);
    let mut samples = vec![0.0; WRITER_DRAIN_SAMPLES - WRITER_DRAIN_SAMPLES % channels];
    loop {
        if mode.load(Ordering::Acquire) == CANCELLED {
            break;
        }
        let popped = consumer.pop_slice(&mut samples);
        if popped > 0 {
            writer.write_samples(&samples[..popped])?;
            continue;
        }
        if mode.load(Ordering::Acquire) != RUNNING {
            break;
        }
        thread::sleep(IDLE_POLL_INTERVAL);
    }
    let state = if mode.load(Ordering::Acquire) == CANCELLED {
        RecordingPrefixState::CancelledRetained
    } else {
        RecordingPrefixState::StoppedUnpublished
    };
    writer.finalize(state)
}

struct BoundedSink {
    inner: Box<dyn RecordingSink>,
    position: u64,
    limit: u64,
}
impl Write for BoundedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .filter(|v| *v <= self.limit)
            .ok_or_else(|| io::Error::other("recording sink byte budget exceeded"))?;
        let written = self.inner.write(bytes)?;
        if written > bytes.len() {
            return Err(io::Error::other("invalid sink write result"));
        }
        self.position = end - (bytes.len() - written) as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl Seek for BoundedSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let offset = match position {
            SeekFrom::Start(v) => i128::from(v),
            SeekFrom::Current(v) => i128::from(self.position) + i128::from(v),
            SeekFrom::End(_) => {
                return Err(io::Error::other(
                    "unbounded end-relative recording seek unsupported",
                ));
            }
        };
        let offset = u64::try_from(offset)
            .ok()
            .filter(|v| *v <= self.limit)
            .ok_or_else(|| io::Error::other("recording seek exceeds retained allocation"))?;
        let observed = self.inner.seek(SeekFrom::Start(offset))?;
        if observed != offset {
            return Err(io::Error::other("recording sink seek result mismatch"));
        }
        self.position = offset;
        Ok(offset)
    }
}

struct WavSampleWriter {
    writer: hound::WavWriter<BufWriter<BoundedSink>>,
    limits: RecordingLimits,
    written_samples: u64,
}
impl WavSampleWriter {
    fn new(
        mut sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
    ) -> Result<Self, AudioInputError> {
        if sink
            .stream_position()
            .map_err(|error| failed(&error.to_string()))?
            != 0
        {
            return Err(failed(
                "recording transport must start at its admitted origin",
            ));
        }
        let spec = hound::WavSpec {
            channels: limits.channels,
            sample_rate: limits.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::new(
            BufWriter::with_capacity(
                16_384,
                BoundedSink {
                    inner: sink,
                    position: 0,
                    limit: limits.max_bytes,
                },
            ),
            spec,
        )
        .map_err(|error| failed(&format!("WAV header unavailable: {error}")))?;
        writer.flush().map_err(|error| {
            failed(&format!(
                "WAV initial header transport unconfirmed: {error}"
            ))
        })?;
        Ok(Self {
            writer,
            limits,
            written_samples: 0,
        })
    }
    fn write_samples(&mut self, samples: &[f32]) -> Result<(), AudioInputError> {
        if samples.len() % usize::from(self.limits.channels) != 0 {
            return Err(failed("incomplete interleaved recording frame"));
        }
        let next = self
            .written_samples
            .checked_add(samples.len() as u64)
            .ok_or_else(|| failed("recording frame budget overflow"))?;
        if next / u64::from(self.limits.channels) > self.limits.max_frames {
            return Err(failed("recording frame budget exceeded; prefix retained"));
        }
        for &sample in samples {
            self.writer
                .write_sample(sample)
                .map_err(|error| failed(&format!("recording sample write failed: {error}")))?;
        }
        self.written_samples = next;
        Ok(())
    }
    fn finalize(self, state: RecordingPrefixState) -> Result<RecordingStats, AudioInputError> {
        self.writer
            .finalize()
            .map_err(|error| failed(&format!("recording header/flush unconfirmed: {error}")))?;
        Ok(RecordingStats {
            frames: self.written_samples / u64::from(self.limits.channels),
            state,
        })
    }
}
fn failed(detail: &str) -> AudioInputError {
    AudioInputError::RecordingFailed {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("writer_tests.rs");
}
