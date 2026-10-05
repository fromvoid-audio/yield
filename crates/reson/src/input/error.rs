//! Input and recording diagnostics; device variants require native audio.
use thiserror::Error;

/// Errors that can occur while enumerating or opening audio inputs.
#[derive(Debug, Error)]
pub enum AudioInputError {
    /// No audio input devices are available on the host.
    #[error("No audio input devices found")]
    NoInputDevices,
    /// Failed to enumerate input devices on the host.
    #[error("Could not list input devices: {source}")]
    #[cfg(feature = "native-audio")]
    ListInputDevices {
        /// Underlying cpal error.
        source: cpal::DevicesError,
    },
    /// Failed to query supported input configs for a host.
    #[error("Failed to read supported configs for {host_id}: {source}")]
    #[cfg(feature = "native-audio")]
    SupportedInputConfigs {
        /// Host identifier used for the query.
        host_id: String,
        /// Underlying cpal error.
        source: cpal::SupportedStreamConfigsError,
    },
    /// Failed to create an input stream.
    #[error("Failed to open input stream: {source}")]
    #[cfg(feature = "native-audio")]
    OpenStream {
        /// Underlying cpal error.
        source: cpal::BuildStreamError,
    },
    /// Failed to read the default input config.
    #[error("Failed to read default input config: {source}")]
    #[cfg(feature = "native-audio")]
    DefaultInputConfig {
        /// Underlying cpal error.
        source: cpal::DefaultStreamConfigError,
    },
    /// Failed to start playback on an input stream.
    #[error("Failed to start input stream: {source}")]
    #[cfg(feature = "native-audio")]
    StartStream {
        /// Underlying cpal error.
        source: cpal::PlayStreamError,
    },
    /// Recording failed due to a runtime error.
    #[error("Recording failed: {detail}")]
    RecordingFailed {
        /// Human-readable failure detail.
        detail: String,
    },
}
