//! The actual bounded serializer without device enumeration or an input stream.
use super::health::{RecordingHealth, RecordingHealthState};
use super::settlement::RecordingSinkBinding;
use super::writer::{RecorderCapture, RecorderWriter, RecordingLimits, RecordingPrefixState};
use crate::input::AudioInputError;
use std::sync::Arc;

/// Completion of a CPU-fed serializer; it proves no physical admission or publication.
pub struct RecordingProducerOutcome {
    /// Frames successfully serialized by the original worker.
    pub frames: u64,
    /// Unpublished prefix disposition.
    pub prefix_state: RecordingPrefixState,
    /// Actual accepted, encoded, abandoned and lost sample observations.
    pub health: RecordingHealth,
}

/// Device-independent controller for the same bounded WAV worker used by native capture.
///
/// The caller must supply an already retained sink and own its admission. Arbitrary codec
/// transports remain useful for tests; this controller grants no filesystem authority.
pub struct RecordingProducer {
    writer: Option<RecorderWriter>,
    capture: Option<RecorderCapture>,
    health: Arc<RecordingHealthState>,
}
impl RecordingProducer {
    /// Consume the exact sink binding and start the original finite serializer.
    /// The paired owner must handle its pre-finalize barrier separately from Stop/join.
    pub fn start(
        binding: RecordingSinkBinding,
        limits: RecordingLimits,
    ) -> Result<Self, AudioInputError> {
        let health = Arc::new(RecordingHealthState::default());
        let (capture, consumer) = RecorderWriter::prepare_capture(limits, health.clone());
        let writer = RecorderWriter::spawn_prepared_with_settlement(
            binding.sink,
            limits,
            consumer,
            capture.mode.clone(),
            health.clone(),
            Some(binding.producer),
        )?;
        Ok(Self {
            writer: Some(writer),
            capture: Some(capture),
            health,
        })
    }

    /// Submit interleaved samples through the original bounded callback-ring accounting.
    /// Ring overflow is observed as input loss; this call never waits for the owner.
    pub fn submit(&mut self, samples: &[f32]) {
        if let Some(capture) = &mut self.capture {
            capture.submit(samples);
        }
    }

    /// Observe current sample accounting; this is neither a seal nor a close capability.
    pub fn health(&self) -> RecordingHealth {
        self.health.snapshot()
    }

    /// Stop submission, drain accepted frames and join after the owner acknowledges.
    pub fn stop(self) -> Result<RecordingProducerOutcome, AudioInputError> {
        self.finish(false)
    }

    /// Stop at the original cancellation barrier and retain an unpublished prefix.
    pub fn cancel(self) -> Result<RecordingProducerOutcome, AudioInputError> {
        self.finish(true)
    }

    fn finish(mut self, cancelled: bool) -> Result<RecordingProducerOutcome, AudioInputError> {
        self.capture.take();
        let mut writer = self.writer.take().expect("live producer owns its worker");
        if cancelled {
            writer.cancel();
        } else {
            writer.stop();
        }
        let stats = writer.join()?;
        Ok(RecordingProducerOutcome {
            frames: stats.frames,
            prefix_state: stats.state,
            health: self.health.snapshot(),
        })
    }
}
impl Drop for RecordingProducer {
    fn drop(&mut self) {
        // Original worker Drop cancels and joins. No finalizer/physical receipt is fabricated.
        self.capture.take();
        self.writer.take();
    }
}

#[cfg(test)]
mod tests;
