#[cfg(feature = "native-audio")]
use std::time::Duration;

#[cfg(feature = "native-audio")]
mod device;
/// Audio input device enumeration and configuration helpers.
#[cfg(feature = "native-audio")]
pub mod input;
/// Device-independent recording diagnostics.
#[cfg(not(feature = "native-audio"))]
#[path = "input/error.rs"]
pub mod input;
/// Audio output device enumeration and stream helpers.
#[cfg(feature = "native-audio")]
pub mod output;
/// Recording and input monitoring helpers.
pub mod recording;

#[cfg(feature = "native-audio")]
mod async_decode;
/// Low-level decoder wrapper for Symphonia.
#[cfg(feature = "native-audio")]
pub mod decoder;
#[cfg(feature = "native-audio")]
mod fade;
#[cfg(feature = "native-audio")]
mod loop_diagnostic;
#[cfg(feature = "native-audio")]
mod mixer;
#[cfg(feature = "native-audio")]
mod player;
#[cfg(feature = "native-audio")]
mod routing;
#[cfg(feature = "native-audio")]
mod source;
#[cfg(feature = "native-audio")]
mod telemetry;
#[cfg(feature = "native-audio")]
mod time_stretch;
#[cfg(feature = "native-audio")]
mod timebase;
/// WAV header repair helpers for streaming and in-memory decode paths.
pub mod wav_sanitize;

#[cfg(not(feature = "native-audio"))]
pub use input::AudioInputError;
#[cfg(feature = "native-audio")]
pub use input::{
    AudioInputConfig, AudioInputError, ResolvedInput, ResolvedInputConfig,
    available_input_channel_count, available_input_devices, available_input_hosts,
    resolve_input_stream_config, supported_input_sample_rates,
};
#[cfg(feature = "native-audio")]
pub use output::{
    AudioDeviceSummary, AudioHostSummary, AudioOutputConfig, AudioOutputError, ResolvedOutput,
    available_devices, available_hosts, open_output_stream, supported_sample_rates,
};
#[cfg(feature = "native-audio")]
pub use player::{
    AudioPlayer, EditFadeRange, FadeParams, PlaybackChannelLayout, PlaybackMetronomeConfig,
    PlaybackRequestId, PlaybackRuntime, PlaybackRuntimeCancellation, PlaybackRuntimeConfig,
    PlaybackRuntimeEvent, PlaybackRuntimeEventIter, PlaybackRuntimeEventReceiver,
    PlaybackRuntimeEventRecvError, PlaybackRuntimeEventRecvTimeoutError,
    PlaybackRuntimeGainNormalization, PlaybackRuntimeHandle, PlaybackRuntimeMode,
    PlaybackRuntimeProgress, PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest,
    PlaybackRuntimeSource, PlaybackRuntimeSpanUpdate, PlaybackRuntimeStarted,
    PlaybackRuntimeStreamPolicy, PlaybackRuntimeSubmitError, PlaybackSeekBehavior,
    PlaybackSourceIdentity, PlaybackSourceKind, PlaybackSpanPlan, PlaybackSpanPlanError,
    PlaybackSpanRequest,
};
#[cfg(feature = "native-audio")]
pub use recording::{AudioRecorder, InputMonitor, RecordingOutcome};
pub use recording::{RecordingHealth, RecordingLimits, RecordingPrefixState, RecordingSink};
#[cfg(feature = "native-audio")]
pub use time_stretch::Wsola;

#[cfg(feature = "native-audio")]
pub(crate) use async_decode::AsyncSource;
#[cfg(test)]
#[cfg(feature = "native-audio")]
pub(crate) use fade::{EdgeFade, FadeOutHandle, FadeOutOnRequest, fade_duration};
#[cfg(test)]
#[cfg(feature = "native-audio")]
pub(crate) use routing::normalized_progress;
#[cfg(feature = "native-audio")]
pub(crate) use source::OutputAdapter;
#[cfg(feature = "native-audio")]
pub use source::{SamplesBuffer, Source};

// Keep the anti-click envelope short enough to preserve sharp audition
// transients while still smoothing abrupt source starts and stops.
#[cfg(feature = "native-audio")]
pub(crate) const DEFAULT_ANTI_CLIP_FADE: Duration = Duration::from_millis(3);

#[cfg(feature = "native-audio")]
#[cfg(test)]
mod tests;
