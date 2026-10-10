use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};

use cpal::traits::HostTrait;
use tracing::info;

use super::discovery::{resolve_device, resolve_host};
use super::{AudioOutputConfig, AudioOutputError, ResolvedOutput};
use crate::device::device_label;

mod builder;
mod config;
mod handle;

#[cfg(test)]
use crate::output::callback::CallbackProbe;
use builder::{BuiltStreamState, build_stream_with_state};
use config::resolve_output_stream_config;
#[cfg(test)]
use config::resolve_output_stream_config_for_test_channels;
pub(super) use config::resolved_output_from_stream_config;
#[cfg(test)]
pub(crate) use handle::monitor_sink_for_tests;
pub use handle::{CpalAudioStream, MonitorSink};

/// Stream creation result that keeps both the stream handle and resolved settings.
pub struct OpenStreamOutcome {
    /// Opened cpal stream with shared state.
    pub stream: CpalAudioStream,
    /// Resolved output configuration used to open the stream.
    pub resolved: ResolvedOutput,
}

/// Open an audio stream honoring user preferences with safe fallbacks.
///
/// On test builds, set `YIELD_TEST_AUDIO_OUTPUT=1` to exercise real output
/// devices; otherwise the function returns `NoOutputDevices` to keep automated
/// test runs deterministic on hosts without stable audio hardware.
pub fn open_output_stream(
    config: &AudioOutputConfig,
) -> Result<OpenStreamOutcome, AudioOutputError> {
    #[cfg(test)]
    {
        if !test_audio_output_enabled() {
            return Err(AudioOutputError::NoOutputDevices);
        }
    }
    let (host, host_id, host_fallback) = resolve_host(config.host.as_deref())?;
    let (device, device_name, device_fallback) = resolve_device(&host, config.device.as_deref())?;

    open_resolved_output_stream(
        device,
        host_id,
        device_name,
        host_fallback || device_fallback,
        config,
        config.host.is_none(),
    )
}

/// Open a test-owned device directly, including CoreAudio devices hidden from
/// the system device enumeration. This is intentionally not a public route or
/// a production fallback path.
#[cfg(test)]
pub(crate) fn open_output_stream_on_test_device(
    device: cpal::Device,
    host_id: String,
    config: &AudioOutputConfig,
    required_channels: u16,
) -> Result<OpenStreamOutcome, String> {
    if !test_audio_output_enabled() {
        return Err("native audio output test is not enabled".into());
    }
    let device_name = device_label(&device).ok_or("native test device has no name")?;
    if config
        .host
        .as_deref()
        .is_some_and(|requested| requested != host_id)
        || config
            .device
            .as_deref()
            .is_some_and(|requested| requested != device_name)
    {
        return Err(
            "native test device identity does not match the requested host and name".into(),
        );
    }
    let resolved_config = resolve_output_stream_config_for_test_channels(
        &device,
        &host_id,
        config,
        required_channels,
    )?;
    open_stream_with_resolved_config(device, host_id, device_name, false, false, resolved_config)
        .map_err(|error| error.to_string())
}

fn open_resolved_output_stream(
    device: cpal::Device,
    resolved_host_id: String,
    resolved_device_name: String,
    used_fallback: bool,
    config: &AudioOutputConfig,
    allow_default_fallback: bool,
) -> Result<OpenStreamOutcome, AudioOutputError> {
    let resolved_config = resolve_output_stream_config(&device, &resolved_host_id, config)?;
    open_stream_with_resolved_config(
        device,
        resolved_host_id,
        resolved_device_name,
        used_fallback,
        allow_default_fallback,
        resolved_config,
    )
}

fn open_stream_with_resolved_config(
    device: cpal::Device,
    mut resolved_host_id: String,
    mut resolved_device_name: String,
    mut used_fallback: bool,
    allow_default_fallback: bool,
    resolved_config: config::ResolvedOutputStreamConfig,
) -> Result<OpenStreamOutcome, AudioOutputError> {
    used_fallback |= resolved_config.used_fallback;

    let active_sources = Arc::new(AtomicUsize::new(0));
    let volume_bits = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
    let clear_pending = Arc::new(AtomicBool::new(false));
    let command_generation = Arc::new(AtomicU64::new(1));
    let closed = Arc::new(AtomicBool::new(false));
    #[cfg(test)]
    let callback_probe = Arc::new(CallbackProbe::new());

    let mut resolved_stream_config = resolved_config.stream_config.clone();
    let BuiltStreamState {
        stream,
        command_sender,
        command_receiver,
        callback_command_producer,
        retired_source_consumer,
        error_receiver,
        error_sender,
    } = match build_stream_with_state(
        &device,
        &resolved_config.stream_config,
        resolved_config.sample_format,
        volume_bits.clone(),
        active_sources.clone(),
        clear_pending.clone(),
        command_generation.clone(),
        closed.clone(),
        #[cfg(test)]
        Arc::clone(&callback_probe),
    ) {
        Ok(stream) => stream,
        Err(err) => {
            if !allow_default_fallback {
                return Err(AudioOutputError::BuildStream { source: err });
            }
            used_fallback = true;
            let default_host = cpal::default_host();
            let fallback_device = default_host.default_output_device().ok_or_else(|| {
                AudioOutputError::BuildStream {
                    source: err.clone(),
                }
            })?;
            resolved_host_id = default_host.id().name().to_string();
            resolved_device_name =
                device_label(&fallback_device).unwrap_or_else(|| "Default device".to_string());

            let fallback_config = resolve_output_stream_config(
                &fallback_device,
                &resolved_host_id,
                &AudioOutputConfig::default(),
            )?;
            resolved_stream_config = fallback_config.stream_config.clone();

            build_stream_with_state(
                &fallback_device,
                &fallback_config.stream_config,
                fallback_config.sample_format,
                volume_bits.clone(),
                active_sources.clone(),
                clear_pending.clone(),
                command_generation.clone(),
                closed.clone(),
                #[cfg(test)]
                Arc::clone(&callback_probe),
            )
            .map_err(|source| AudioOutputError::BuildDefaultStream { source })?
        }
    };

    let resolved = resolved_output_from_stream_config(
        resolved_host_id,
        resolved_device_name,
        &resolved_stream_config,
        used_fallback,
    );
    info!(
        "Audio output ready: host={} device=\"{}\" rate={}Hz channels={} buffer={:?} fallback={}",
        resolved.host_id,
        resolved.device_name,
        resolved.sample_rate,
        resolved.channel_count,
        resolved.buffer_size_frames,
        resolved.used_fallback
    );
    let stream = CpalAudioStream::new(
        stream,
        command_sender,
        command_receiver,
        callback_command_producer,
        retired_source_consumer,
        error_sender,
        closed,
        active_sources,
        volume_bits,
        error_receiver,
        clear_pending,
        command_generation,
        #[cfg(test)]
        callback_probe,
    )?;
    Ok(OpenStreamOutcome { stream, resolved })
}

#[cfg(test)]
fn test_audio_output_enabled() -> bool {
    std::env::var("YIELD_TEST_AUDIO_OUTPUT")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}
