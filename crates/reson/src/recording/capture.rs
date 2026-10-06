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
    settlement: Option<super::settlement::ProducerSettlement>,
) -> Result<RecordingRuntime, AudioInputError> {
    let resolved = resolve_input_stream_config(config)?;
    let selection =
        StreamChannelSelection::new(resolved.stream_config.channels, &resolved.selected_channels);
    let limits = resolved_recording_limits(
        resolved.resolved.sample_rate,
        resolved.resolved.recorded_channel_count,
        max_frames,
        settlement.as_ref(),
    )?;
    let health = Arc::new(RecordingHealthState::default());
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
    let writer = RecorderWriter::spawn_prepared_with_settlement(
        sink,
        limits,
        consumer,
        writer_mode,
        Arc::clone(&health),
        settlement,
    )?;
    Ok(RecordingRuntime {
        stream,
        writer,
        resolved: resolved.resolved,
        monitor,
        health,
    })
}

// Called with the actual negotiated input format, before callback/monitor rings,
// paused stream construction and serializer header writes.
fn resolved_recording_limits(
    sample_rate: u32,
    channels: u16,
    max_frames: u64,
    settlement: Option<&super::settlement::ProducerSettlement>,
) -> Result<RecordingLimits, AudioInputError> {
    let limits = RecordingLimits::new(sample_rate, channels, max_frames)?;
    if let Some(settlement) = settlement {
        settlement.validate_limits(limits)?;
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{RecordingSinkBinding, RecordingTransport};

    #[test]
    fn actual_resolved_startup_guard_preserves_original_format_and_generic_codec_behavior() {
        let original = RecordingLimits::new(48_000, 1, 8192).unwrap();
        let (binding, _, _) = RecordingTransport::bind(original);
        assert_eq!(
            resolved_recording_limits(48_000, 1, 8192, Some(&binding.producer)).unwrap(),
            original
        );
        for (rate, channels, frames) in [
            (192_000, 1, 8192),
            (48_000, 2, 4096),
            (48_000, 1, 4096),
            (192_000, 8, 1024),
        ] {
            assert!(
                resolved_recording_limits(rate, channels, frames, Some(&binding.producer)).is_err()
            );
        }
        let (codec, _) = RecordingSinkBinding::bind(Box::new(std::io::Cursor::new(Vec::new())));
        assert!(resolved_recording_limits(192_000, 8, 1024, Some(&codec.producer)).is_ok());
        assert!(resolved_recording_limits(192_000, 8, 1024, None).is_ok());
    }
}
