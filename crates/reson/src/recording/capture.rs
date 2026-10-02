//! CPAL recording bootstrap and capture-loop wiring.

use std::sync::Arc;

use cpal::Stream;

use super::health::RecordingHealthState;
use super::monitor::{RecordingMonitor, start_recording_monitor};
use super::writer::{RecorderWriter, RecordingLimits, RecordingSink};
use crate::input::{
    AudioInputConfig, AudioInputError, ResolvedInput, StreamChannelSelection, build_input_stream,
    resolve_input_stream_config,
};

/// Bundled runtime pieces returned when a recording session starts successfully.
pub(super) struct RecordingRuntime {
    pub(super) stream: Stream,
    pub(super) writer: RecorderWriter,
    pub(super) resolved: ResolvedInput,
    pub(super) monitor: RecordingMonitor,
    pub(super) health: Arc<RecordingHealthState>,
}

/// Resolve input settings, start the WAV writer, and build the live capture stream.
pub(super) fn start_recording_runtime(
    config: &AudioInputConfig,
    sink: Box<dyn RecordingSink>,
    max_frames: u64,
) -> Result<RecordingRuntime, AudioInputError> {
    let resolved = resolve_input_stream_config(config)?;
    let selection =
        StreamChannelSelection::new(resolved.stream_config.channels, &resolved.selected_channels);
    let health = Arc::new(RecordingHealthState::default());
    let limits = RecordingLimits::new(
        resolved.resolved.sample_rate,
        resolved.resolved.recorded_channel_count,
        max_frames,
    )?;
    let (mut writer_capture, consumer) =
        RecorderWriter::prepare_capture(limits, Arc::clone(&health));
    let writer_mode = Arc::clone(&writer_capture.mode);
    let (monitor, mut monitor_capture) = start_recording_monitor(
        resolved.resolved.sample_rate,
        resolved.resolved.recorded_channel_count,
        Arc::clone(&health),
    );
    let stream = build_input_stream(
        &resolved.device,
        &resolved.stream_config,
        resolved.sample_format,
        selection,
        move |samples| {
            writer_capture.submit(samples);
            monitor_capture.submit(samples);
        },
    )?;
    let writer =
        RecorderWriter::spawn_prepared(sink, limits, consumer, writer_mode, Arc::clone(&health))?;
    Ok(RecordingRuntime {
        stream,
        writer,
        resolved: resolved.resolved,
        monitor,
        health,
    })
}
