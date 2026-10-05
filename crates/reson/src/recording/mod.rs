//! Audio recording pipeline and monitoring utilities.

#[cfg(feature = "native-audio")]
mod capture;
mod health;
#[cfg(feature = "native-audio")]
mod monitor;
mod producer;
mod settlement;
mod writer;
pub use producer::{RecordingProducer, RecordingProducerOutcome};

#[cfg(all(test, feature = "native-audio"))]
mod sink_contract_tests;

#[cfg(feature = "native-audio")]
use std::path::{Path, PathBuf};

#[cfg(feature = "native-audio")]
use cpal::Stream;
#[cfg(feature = "native-audio")]
use cpal::traits::StreamTrait;

#[cfg(feature = "native-audio")]
use super::input::{AudioInputConfig, AudioInputError, ResolvedInput};
#[cfg(feature = "native-audio")]
use std::sync::Arc;

#[cfg(feature = "native-audio")]
use health::RecordingHealthState;
#[cfg(feature = "native-audio")]
use monitor::RecordingMonitor;
#[cfg(feature = "native-audio")]
use writer::RecorderWriter;

pub use health::RecordingHealth;
pub use settlement::{
    RECORDING_TERMINAL_ACK_TIMEOUT, RecordingJoinedCompletion, RecordingSettlements,
    RecordingSinkBinding, RecordingTerminalAccounting, RecordingTerminalBarrier,
    RecordingTerminalWriteInventory,
};
pub use writer::{RecordingLimits, RecordingPrefixState, RecordingSink};

/// Summary returned when a recording completes.
#[cfg(feature = "native-audio")]
pub struct RecordingOutcome {
    /// Diagnostic target locator; codec completion does not prove publication or reopen it.
    pub path: PathBuf,
    /// Input configuration used for the capture.
    pub resolved: ResolvedInput,
    /// Number of audio frames written.
    pub frames: u64,
    /// Duration of the recording in seconds.
    pub duration_seconds: f32,
    /// Capture overrun and worker-failure counters for the completed session.
    pub health: RecordingHealth,
    /// Unpublished prefix disposition; only the app owner may validate and publish staging.
    pub prefix_state: RecordingPrefixState,
}

/// Active audio recorder that streams samples to a WAV file.
#[cfg(feature = "native-audio")]
pub struct AudioRecorder {
    stream: Option<Stream>,
    writer: Option<RecorderWriter>,
    resolved: ResolvedInput,
    path: PathBuf,
    monitor: RecordingMonitor,
    health: Arc<RecordingHealthState>,
    active: bool,
}

#[cfg(feature = "native-audio")]
impl AudioRecorder {
    /// Unadmitted pathname startup is unsupported and never opens/truncates an output.
    /// Use an already retained sink through the app's actual owner/admission adapter.
    pub fn start(_config: &AudioInputConfig, _path: PathBuf) -> Result<Self, AudioInputError> {
        Err(AudioInputError::RecordingFailed {
            detail: "Recording requires an admitted retained sink; pathname startup unsupported"
                .into(),
        })
    }

    /// Encode a finite capture into an already retained byte transport, without publication.
    /// The app owns filesystem admission/collision/closure; `diagnostic_path` is never opened.
    pub fn start_into_sink(
        config: &AudioInputConfig,
        sink: Box<dyn RecordingSink>,
        max_frames: u64,
        diagnostic_path: PathBuf,
    ) -> Result<Self, AudioInputError> {
        let runtime = capture::start_recording_runtime(config, sink, max_frames, None)?;
        runtime
            .stream
            .play()
            .map_err(|source| AudioInputError::StartStream { source })?;
        Ok(Self {
            stream: Some(runtime.stream),
            writer: Some(runtime.writer),
            resolved: runtime.resolved,
            path: diagnostic_path,
            monitor: runtime.monitor,
            health: runtime.health,
            active: true,
        })
    }

    /// Start the actual paused-input/serializer route with a previously bound settlement channel.
    /// The admitted adapter must handle its receiver on another thread before calling Stop/join.
    /// Producer evidence alone does not activate journal, staging or publication authority.
    pub fn start_into_bound_sink(
        config: &AudioInputConfig,
        binding: RecordingSinkBinding,
        max_frames: u64,
        diagnostic_path: PathBuf,
    ) -> Result<Self, AudioInputError> {
        let runtime = capture::start_recording_runtime(
            config,
            binding.sink,
            max_frames,
            Some(binding.producer),
        )?;
        runtime
            .stream
            .play()
            .map_err(|source| AudioInputError::StartStream { source })?;
        Ok(Self {
            stream: Some(runtime.stream),
            writer: Some(runtime.writer),
            resolved: runtime.resolved,
            path: diagnostic_path,
            monitor: runtime.monitor,
            health: runtime.health,
            active: true,
        })
    }

    /// Stop recording and return summary metadata.
    pub fn stop(&mut self) -> Result<RecordingOutcome, AudioInputError> {
        if !self.active {
            return Err(AudioInputError::RecordingFailed {
                detail: "Recorder already stopped".into(),
            });
        }
        self.active = false;
        drop(self.stream.take());
        self.monitor.stop();
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| AudioInputError::RecordingFailed {
                detail: "Recorder writer unavailable".into(),
            })?;
        writer.stop();
        let stats = writer.join()?;
        let duration_seconds = if stats.frames == 0 {
            0.0
        } else {
            stats.frames as f32 / self.resolved.sample_rate.max(1) as f32
        };
        Ok(RecordingOutcome {
            path: self.path.clone(),
            resolved: self.resolved.clone(),
            frames: stats.frames,
            duration_seconds,
            health: self.health(),
            prefix_state: stats.state,
        })
    }

    /// Cancel at a bounded writer barrier and retain the unpublished prefix; never delete a path.
    pub fn cancel(&mut self) -> Result<RecordingOutcome, AudioInputError> {
        if let Some(writer) = self.writer.as_mut() {
            writer.cancel();
        }
        self.stop()
    }

    /// Return true while the recorder is actively capturing audio.
    pub fn is_active(&self) -> bool {
        self.active
            && !self
                .health
                .writer_failed
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Return the resolved input configuration used for recording.
    pub fn resolved(&self) -> &ResolvedInput {
        &self.resolved
    }

    /// Return the output path for the recording.
    pub fn output_path(&self) -> &Path {
        &self.path
    }

    /// Return a lock-free snapshot of capture overruns and worker failures.
    pub fn health(&self) -> RecordingHealth {
        self.health.snapshot()
    }

    /// Attach a monitor that receives live audio samples.
    pub fn attach_monitor(&self, monitor: &monitor::InputMonitor) {
        self.monitor.attach(monitor.target());
    }

    /// Detach any active input monitor.
    pub fn detach_monitor(&self) {
        self.monitor.detach();
    }
}

#[cfg(feature = "native-audio")]
pub use monitor::InputMonitor;

#[cfg(all(test, feature = "native-audio"))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn unadmitted_pathname_start_preserves_existing_absent_and_symlink_outputs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recording.wav");
        std::fs::write(&path, b"preserve original").unwrap();
        let result = AudioRecorder::start(&AudioInputConfig::default(), path.clone());
        assert!(
            matches!(result, Err(AudioInputError::RecordingFailed { detail }) if detail.contains("admitted retained sink"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve original");
        let absent = dir.path().join("absent.wav");
        assert!(AudioRecorder::start(&AudioInputConfig::default(), absent.clone()).is_err());
        assert!(!absent.exists());
        #[cfg(unix)]
        {
            let link = dir.path().join("linked.wav");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(AudioRecorder::start(&AudioInputConfig::default(), link.clone()).is_err());
            assert!(
                std::fs::symlink_metadata(link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read(path).unwrap(), b"preserve original");
        }
    }

    #[test]
    fn recorder_is_active_clears_after_stop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recording.wav");
        let health = Arc::new(RecordingHealthState::default());
        let (writer, _capture) = RecorderWriter::spawn(
            Box::new(std::io::Cursor::new(Vec::<u8>::new())),
            RecordingLimits::new(48_000, 2, 100).unwrap(),
            Arc::clone(&health),
        )
        .unwrap();
        let (monitor, _monitor_capture) =
            monitor::start_recording_monitor(48_000, 2, Arc::clone(&health));
        let resolved = ResolvedInput {
            host_id: "test".to_string(),
            device_name: "test device".to_string(),
            sample_rate: 48_000,
            buffer_size_frames: Some(128),
            stream_channel_count: 2,
            recorded_channel_count: 2,
            selected_channels: vec![1, 2],
            used_fallback: false,
        };
        let mut recorder = AudioRecorder {
            stream: None,
            writer: Some(writer),
            resolved,
            path,
            monitor,
            health,
            active: true,
        };

        assert!(recorder.is_active());
        let outcome = recorder.stop().unwrap();
        assert!(!recorder.is_active());
        assert_eq!(outcome.frames, 0);
        assert_eq!(
            outcome.health,
            RecordingHealth {
                writer_accounting_complete: true,
                ..RecordingHealth::default()
            }
        );
    }
}
