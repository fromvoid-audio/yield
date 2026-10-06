//! Bounded background WAV serialization into an already retained transport.
//!
//! No pathname opens or publication authority live here. The app's owner adapter remains
//! responsible for admission, retained staging identity, sync/validation and publication.
use super::health::RecordingHealthState;
use super::settlement::{
    BarrierIssuer, JoinedIssuer, ProducerSettlement, RecordingTerminalWriteInventory,
    TerminalFacts, TerminalWriteFence,
};
use crate::input::AudioInputError;
use ringbuf::{HeapRb, traits::*};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const WRITER_DRAIN_SAMPLES: usize = 4096;
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const RUNNING: u8 = 0;
pub(super) const STOPPED: u8 = 1;
pub(super) const CANCELLED: u8 = 2;

#[cfg(test)]
thread_local! {
    pub(super) static PREPARED_RING_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
    /// Actual validated serializer sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    /// Actual validated interleaved serializer channel count.
    pub fn channels(&self) -> u16 {
        self.channels
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

#[derive(Clone, Debug)]
pub(super) struct RecordingStats {
    pub(super) frames: u64,
    pub(super) state: RecordingPrefixState,
    terminal: Option<TerminalFacts>,
}

pub(super) struct RecorderCapture {
    producer: ringbuf::HeapProd<f32>,
    health: Arc<RecordingHealthState>,
    pub(super) mode: Arc<AtomicU8>,
    channels: usize,
    #[cfg(test)]
    submission_hook: Option<Box<dyn FnOnce() + Send>>,
}

struct CaptureSubmission<'a>(&'a RecordingHealthState);
impl Drop for CaptureSubmission<'_> {
    fn drop(&mut self) {
        self.0.capture_in_flight.store(false, Ordering::SeqCst);
    }
}

impl RecorderCapture {
    pub(super) fn submit(&mut self, samples: &[f32]) {
        // These SC operations and the terminal worker's SC mode RMW prevent both sides from
        // missing one another: an accepted submission finishes before terminal accounting.
        self.health.capture_in_flight.store(true, Ordering::SeqCst);
        let _submission = CaptureSubmission(&self.health);
        if samples.len() % self.channels != 0 {
            self.health.writer_failed.store(true, Ordering::Release);
            self.mode.store(CANCELLED, Ordering::Release);
        }
        if self.mode.load(Ordering::SeqCst) != RUNNING
            || self.health.writer_failed.load(Ordering::Acquire)
            || self.producer.vacant_len() < samples.len()
        {
            self.health
                .add_writer_count(&self.health.writer_dropped_samples, samples.len() as u64);
            self.health
                .add_writer_count(&self.health.writer_overrun_events, 1);
            return;
        }
        #[cfg(test)]
        if let Some(hook) = self.submission_hook.take() {
            hook();
        }
        let pushed = self.producer.push_slice(samples);
        debug_assert_eq!(pushed, samples.len());
        self.health
            .add_writer_count(&self.health.writer_accepted_samples, pushed as u64);
    }
}

pub(super) struct RecorderWriter {
    mode: Arc<AtomicU8>,
    join: Option<JoinHandle<Result<RecordingStats, AudioInputError>>>,
    joined_issuer: Option<JoinedIssuer>,
}
impl RecorderWriter {
    /// Prepare callback state without opening or writing a transport.
    pub(super) fn prepare_capture(
        limits: RecordingLimits,
        health: Arc<RecordingHealthState>,
    ) -> (RecorderCapture, ringbuf::HeapCons<f32>) {
        let (producer, consumer) = HeapRb::<f32>::new(limits.ring_samples).split();
        #[cfg(test)]
        PREPARED_RING_BYTES.with(|bytes| bytes.set(limits.ring_samples * size_of::<f32>()));
        (
            RecorderCapture {
                producer,
                health,
                mode: Arc::new(AtomicU8::new(RUNNING)),
                channels: usize::from(limits.channels),
                #[cfg(test)]
                submission_hook: None,
            },
            consumer,
        )
    }

    /// Legacy observation fixture route; production startup uses the same optional-settlement core.
    #[cfg(test)]
    pub(super) fn spawn_prepared(
        sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
        consumer: ringbuf::HeapCons<f32>,
        mode: Arc<AtomicU8>,
        health: Arc<RecordingHealthState>,
    ) -> Result<Self, AudioInputError> {
        Self::spawn_prepared_with_settlement(sink, limits, consumer, mode, health, None)
    }

    pub(super) fn spawn_prepared_with_settlement(
        sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
        consumer: ringbuf::HeapCons<f32>,
        mode: Arc<AtomicU8>,
        health: Arc<RecordingHealthState>,
        settlement: Option<ProducerSettlement>,
    ) -> Result<Self, AudioInputError> {
        let (barrier, joined_issuer) = match settlement {
            Some(producer) => {
                let (barrier, joined) = producer.start(mode.clone(), health.clone());
                (Some(barrier), Some(joined))
            }
            None => (None, None),
        };
        let writer = WavSampleWriter::new_with_barrier(sink, limits, barrier)?;
        let worker_mode = Arc::clone(&mode);
        let join = thread::Builder::new()
            .name("reson-recording-writer".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    writer_loop(writer, consumer, &worker_mode, &health)
                }));
                let result = match result {
                    Ok(result) => result,
                    Err(panic) => {
                        health.writer_failed.store(true, Ordering::Release);
                        health
                            .writer_accounting_complete
                            .store(false, Ordering::Release);
                        worker_mode.store(CANCELLED, Ordering::SeqCst);
                        std::panic::resume_unwind(panic);
                    }
                };
                if result.is_err() {
                    health.writer_failed.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|error| failed(&format!("recorder thread unavailable: {error}")))?;
        Ok(Self {
            mode,
            join: Some(join),
            joined_issuer,
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
        let result = self
            .join
            .take()
            .ok_or_else(|| failed("recorder writer already joined"))?
            .join()
            .map_err(|_| failed("recorder writer panicked; prefix is unconfirmed"))?;
        let issuer = self.joined_issuer.take();
        let stats = result?;
        if let Some(issuer) = issuer {
            issuer.publish(
                stats
                    .terminal
                    .clone()
                    .ok_or_else(|| failed("recording terminal proof unavailable"))?,
            )?;
        }
        Ok(stats)
    }
}
impl Drop for RecorderWriter {
    fn drop(&mut self) {
        if let Some(issuer) = self.joined_issuer.take() {
            issuer.deny();
        }
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
    health: &RecordingHealthState,
) -> Result<RecordingStats, AudioInputError> {
    // A read batch must remain an exact number of interleaved frames for every accepted format.
    let channels = usize::from(writer.limits.channels);
    let mut samples = vec![0.0; WRITER_DRAIN_SAMPLES - WRITER_DRAIN_SAMPLES % channels];
    loop {
        if mode.load(Ordering::Acquire) == CANCELLED {
            settle_capture(mode, health);
            break;
        }
        let popped = consumer.pop_slice(&mut samples);
        if popped > 0 {
            if let Err(error) = writer.write_samples(&samples[..popped]) {
                mode.store(CANCELLED, Ordering::SeqCst);
                settle_capture(mode, health);
                health.add_writer_count(&health.writer_unconfirmed_samples, popped as u64);
                health.add_writer_count(
                    &health.writer_abandoned_samples,
                    consumer.occupied_len() as u64,
                );
                health.writer_failed.store(true, Ordering::Release);
                health
                    .writer_accounting_complete
                    .store(true, Ordering::Release);
                return Err(error);
            }
            health.add_writer_count(&health.writer_encoded_samples, popped as u64);
            continue;
        }
        if mode.load(Ordering::Acquire) != RUNNING {
            settle_capture(mode, health);
            // A callback may have passed Running before Stop. Drain its accepted frames too.
            if mode.load(Ordering::Acquire) == STOPPED && consumer.occupied_len() != 0 {
                continue;
            }
            break;
        }
        thread::sleep(IDLE_POLL_INTERVAL);
    }
    let state = if mode.load(Ordering::Acquire) == CANCELLED {
        RecordingPrefixState::CancelledRetained
    } else {
        RecordingPrefixState::StoppedUnpublished
    };
    health.add_writer_count(
        &health.writer_abandoned_samples,
        consumer.occupied_len() as u64,
    );
    if writer.barrier.is_some() {
        health
            .writer_accounting_complete
            .store(true, Ordering::Release);
        let facts = BarrierIssuer::facts(
            state,
            writer.written_samples / u64::from(writer.limits.channels),
            writer.limits,
            health,
            writer.prepare_terminal_inventory()?,
        )?;
        writer
            .barrier
            .as_ref()
            .unwrap()
            .acknowledge(facts.clone(), health)?;
        writer.terminal = Some(facts);
    }
    let result = writer.finalize(state);
    if result.is_err() {
        health.writer_failed.store(true, Ordering::Release);
    }
    health
        .writer_accounting_complete
        .store(true, Ordering::Release);
    result
}

fn settle_capture(mode: &AtomicU8, health: &RecordingHealthState) {
    // Preserve Stop vs Cancel, while putting terminal mode in the SC handshake order.
    let terminal_mode = mode.fetch_or(0, Ordering::SeqCst);
    debug_assert_ne!(terminal_mode, RUNNING);
    while health.capture_in_flight.load(Ordering::SeqCst) {
        thread::sleep(IDLE_POLL_INTERVAL);
    }
}

struct BoundedSink {
    inner: Box<dyn RecordingSink>,
    position: u64,
    limit: u64,
    terminal_fence: Option<Arc<TerminalWriteFence>>,
    terminal_bytes: Option<TerminalByteCursor>,
}
impl BoundedSink {
    fn transport_error(&self, error: io::Error) -> io::Error {
        if let Some(fence) = &self.terminal_fence {
            fence.deny();
        }
        error
    }
}
impl Write for BoundedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(fence) = &self.terminal_fence {
            fence.check()?;
        }
        if let Some(cursor) = &self.terminal_bytes {
            cursor.check_write(self.position, bytes)?;
        }
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .filter(|v| *v <= self.limit)
            .ok_or_else(|| io::Error::other("recording sink byte budget exceeded"))?;
        let written = self
            .inner
            .write(bytes)
            .map_err(|error| self.transport_error(error))?;
        if written > bytes.len() {
            return Err(self.transport_error(io::Error::other("invalid sink write result")));
        }
        self.position = end - (bytes.len() - written) as u64;
        if let Some(cursor) = &mut self.terminal_bytes {
            cursor.written += written;
        }
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(fence) = &self.terminal_fence {
            fence.check()?;
        }
        self.inner
            .flush()
            .map_err(|error| self.transport_error(error))
    }
}
impl Seek for BoundedSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if let Some(fence) = &self.terminal_fence {
            fence.check()?;
        }
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
        if let Some(cursor) = &mut self.terminal_bytes {
            cursor.seek(offset)?;
        }
        let observed = self
            .inner
            .seek(SeekFrom::Start(offset))
            .map_err(|error| self.transport_error(error))?;
        if observed != offset {
            return Err(
                self.transport_error(io::Error::other("recording sink seek result mismatch"))
            );
        }
        self.position = offset;
        Ok(offset)
    }
}

// The real std BufWriter stays private and single-producer. The second Arc is only an
// inspector retained by WavSampleWriter, never a caller/owner transport handle.
#[derive(Clone)]
struct InspectableBuffer(Arc<Mutex<BufWriter<BoundedSink>>>);
impl Write for InspectableBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("recording buffer poisoned"))?
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("recording buffer poisoned"))?
            .flush()
    }
}
impl Seek for InspectableBuffer {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("recording buffer poisoned"))?
            .seek(position)
    }
}

// Enforce the exact pinned hound finalization trace against the actual privately captured
// pending buffer. Partial writes advance only their confirmed prefix. No extra patch/retry
// becomes a successful join, including implicit hound Drop after a failed finalization.
struct TerminalByteCursor {
    inventory: Arc<RecordingTerminalWriteInventory>,
    stage: u8,
    written: usize,
}
impl TerminalByteCursor {
    fn expected(&self) -> (u64, &[u8]) {
        match self.stage {
            0 => (self.inventory.pending_offset, &self.inventory.pending),
            1 => (4, &self.inventory.riff_length),
            _ => (64, &self.inventory.data_length),
        }
    }
    fn check_write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let (start, expected) = self.expected();
        if offset != start + self.written as u64
            || expected.get(self.written..self.written.saturating_add(bytes.len())) != Some(bytes)
            || bytes.is_empty()
        {
            return Err(io::Error::other(
                "recording terminal bytes differ from frozen producer inventory",
            ));
        }
        Ok(())
    }
    fn seek(&mut self, offset: u64) -> io::Result<()> {
        if self.written != self.expected().1.len()
            || !matches!((self.stage, offset), (0, 4) | (1, 64))
        {
            return Err(io::Error::other(
                "recording terminal seek differs from frozen producer inventory",
            ));
        }
        self.stage += 1;
        self.written = 0;
        Ok(())
    }
    fn complete(&self) -> bool {
        self.stage == 2 && self.written == 4
    }
}

struct WavSampleWriter {
    writer: Option<hound::WavWriter<InspectableBuffer>>,
    buffer: InspectableBuffer,
    barrier: Option<BarrierIssuer>,
    terminal: Option<TerminalFacts>,
    limits: RecordingLimits,
    written_samples: u64,
}
impl WavSampleWriter {
    #[cfg(test)]
    fn new(sink: Box<dyn RecordingSink>, limits: RecordingLimits) -> Result<Self, AudioInputError> {
        Self::new_with_barrier(sink, limits, None)
    }
    fn new_with_barrier(
        mut sink: Box<dyn RecordingSink>,
        limits: RecordingLimits,
        barrier: Option<BarrierIssuer>,
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
        let buffer = InspectableBuffer(Arc::new(Mutex::new(BufWriter::with_capacity(
            16_384,
            BoundedSink {
                inner: sink,
                position: 0,
                limit: limits.max_bytes,
                terminal_fence: barrier.as_ref().map(|issuer| issuer.guard.clone()),
                terminal_bytes: None,
            },
        ))));
        let mut writer = hound::WavWriter::new(buffer.clone(), spec)
            .map_err(|error| failed(&format!("WAV header unavailable: {error}")))?;
        writer.flush().map_err(|error| {
            failed(&format!(
                "WAV initial header transport unconfirmed: {error}"
            ))
        })?;
        Ok(Self {
            writer: Some(writer),
            buffer,
            barrier,
            terminal: None,
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
                .as_mut()
                .unwrap()
                .write_sample(sample)
                .map_err(|error| failed(&format!("recording sample write failed: {error}")))?;
        }
        self.written_samples = next;
        Ok(())
    }
    fn prepare_terminal_inventory(
        &self,
    ) -> Result<Arc<RecordingTerminalWriteInventory>, AudioInputError> {
        let mut buffer = self
            .buffer
            .0
            .lock()
            .map_err(|_| failed("recording buffer poisoned"))?;
        let pcm_bytes = self
            .written_samples
            .checked_mul(4)
            .ok_or_else(|| failed("recording extent overflow"))?;
        let object_bytes = pcm_bytes
            .checked_add(68)
            .filter(|n| *n <= self.limits.max_bytes && *n - 8 <= u64::from(u32::MAX))
            .ok_or_else(|| failed("recording extent exceeds finite format"))?;
        let pending_offset = buffer.get_ref().position;
        if buffer.capacity() != 16_384
            || buffer.buffer().len() > 16_384
            || pending_offset < 68
            || pending_offset % 4 != 0
            || pending_offset.checked_add(buffer.buffer().len() as u64) != Some(object_bytes)
            || buffer.get_ref().terminal_bytes.is_some()
        {
            return Err(failed(
                "recording serialized buffer differs from terminal coverage",
            ));
        }
        let inventory = Arc::new(RecordingTerminalWriteInventory {
            pending_offset,
            pending: buffer.buffer().to_vec(),
            object_bytes,
            riff_length: u32::try_from(object_bytes - 8)
                .map_err(|_| failed("RIFF length overflow"))?
                .to_le_bytes(),
            data_length: u32::try_from(pcm_bytes)
                .map_err(|_| failed("data length overflow"))?
                .to_le_bytes(),
        });
        buffer.get_mut().terminal_bytes = Some(TerminalByteCursor {
            inventory: inventory.clone(),
            stage: 0,
            written: 0,
        });
        Ok(inventory)
    }
    fn finalize(mut self, state: RecordingPrefixState) -> Result<RecordingStats, AudioInputError> {
        self.writer
            .take()
            .unwrap()
            .finalize()
            .map_err(|error| failed(&format!("recording header/flush unconfirmed: {error}")))?;
        if self.terminal.is_some()
            && !self
                .buffer
                .0
                .lock()
                .map_err(|_| failed("recording buffer poisoned"))?
                .get_ref()
                .terminal_bytes
                .as_ref()
                .is_some_and(TerminalByteCursor::complete)
        {
            return Err(failed("recording terminal byte inventory incomplete"));
        }
        Ok(RecordingStats {
            frames: self.written_samples / u64::from(self.limits.channels),
            state,
            terminal: self.terminal.take(),
        })
    }
}
impl Drop for WavSampleWriter {
    fn drop(&mut self) {
        // Arm before hound/BufWriter field destructors, including codec errors and unwinding.
        if let Some(barrier) = &self.barrier {
            barrier.guard.deny();
        }
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
