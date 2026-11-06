use std::{
    collections::VecDeque,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::TryRecvError,
    },
    thread,
    time::Duration,
};

#[cfg(test)]
use std::sync::mpsc;

use super::metronome::MetronomeMixOverride;
use super::{AudioPlayer, EditFadeRange, PlaybackMetronomeConfig, PlaybackMetronomeMix};
use crate::output::ResolvedOutput;

mod tempo;
pub use tempo::PlaybackRuntimeTempoSpan;
use tempo::TempoPlaybackTimeline;

#[cfg(all(test, target_os = "macos", feature = "native-audio"))]
#[path = "../output/native_test_fixture.rs"]
mod native_test_fixture;

const DEFAULT_PLAYBACK_COMMAND_QUEUE: usize = 8;
// Keep one bounded cancellation slot available for a priority command that
// must evict a queued play after ordinary coalescing has saturated records.
const CRITICAL_CANCELLATION_RESERVE: usize = 1;
const F32_SAMPLE_BYTES: usize = std::mem::size_of::<f32>();
const NORMALIZED_GAIN_READ_SAMPLES: usize = 4096;

/// Monotonic id assigned to playback runtime commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaybackRequestId(u64);

impl PlaybackRequestId {
    /// Return the numeric id for persistence-free correlation by host apps.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Playback runtime queue configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackRuntimeConfig {
    /// Maximum number of commands or events retained by the runtime queues.
    ///
    /// Submission remains non-blocking. Repeated disposable commands are
    /// coalesced while stop, cancel, and shutdown commands are admitted ahead
    /// of disposable work when the queue is saturated.
    pub queue_capacity: usize,
}

impl Default for PlaybackRuntimeConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_PLAYBACK_COMMAND_QUEUE,
        }
    }
}

/// Decoder buffering policy for lazy/file-backed playback sources.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRuntimeStreamPolicy {
    /// Maximum ring capacity in seconds of source audio.
    pub buffer_seconds: f32,
    /// Startup and post-starvation refill threshold, rounded to whole source frames.
    pub prefill_duration: Duration,
    /// Maximum initial wait for prefill on the runtime owner thread.
    pub prefill_timeout: Duration,
}

impl PlaybackRuntimeStreamPolicy {
    pub const fn full() -> Self {
        Self {
            buffer_seconds: 1.0,
            prefill_duration: Duration::from_millis(5),
            prefill_timeout: Duration::from_millis(5),
        }
    }

    pub const fn transient_navigation() -> Self {
        // Rapid replacement needs a shorter startup wait and a smaller decode cushion.
        Self {
            buffer_seconds: 0.12,
            prefill_duration: Duration::from_millis(3),
            prefill_timeout: Duration::from_millis(3),
        }
    }
}

impl Default for PlaybackRuntimeStreamPolicy {
    fn default() -> Self {
        Self::full()
    }
}

/// Audio source payload and timing metadata for a playback command.
#[derive(Clone, Debug)]
pub enum PlaybackRuntimeSource {
    /// Original encoded bytes with caller-provided timing metadata.
    AudioBytes {
        data: Arc<[u8]>,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    },
    /// Encoded audio file path with caller-provided timing metadata.
    AudioFile {
        path: PathBuf,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    },
    /// WAV file path whose timing metadata is probed on the playback runtime thread.
    ///
    /// Hosts can submit this variant from latency-sensitive UI paths without
    /// opening the file or decoding audio before the command is enqueued.
    WavFile { path: PathBuf },
    /// Uncompressed AIFF file with timing metadata probed on the runtime thread.
    AiffFile { path: PathBuf },
    /// Original encoded bytes plus pre-decoded interleaved f32 playback samples.
    DecodedSamples {
        audio_bytes: Arc<[u8]>,
        samples: Arc<[f32]>,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    },
    /// Tempo-adjusted PCM with exact original-file coordinates.
    ///
    /// The request's mode selects one-shot or looped playback. Span and loop offset come from
    /// this payload. Edit previews and source-anchored metronomes require the ordinary source.
    PreparedTempo(PlaybackRuntimeTempoSpan),
    /// Raw little-endian interleaved f32 file with caller-provided timing metadata.
    InterleavedF32File {
        path: PathBuf,
        sample_count: u64,
        duration: f32,
        sample_rate: u32,
        channels: usize,
    },
}

impl PlaybackRuntimeSource {
    /// Select the background-probed WAV or AIFF file path without opening the source.
    pub fn probe_file(path: PathBuf) -> Self {
        if yield_aiff::is_aiff_path(&path) {
            Self::AiffFile { path }
        } else {
            Self::WavFile { path }
        }
    }

    fn apply_to_player(self, player: &mut AudioPlayer) -> Result<(), String> {
        match self {
            Self::AudioBytes {
                data,
                duration,
                sample_rate,
                channels,
            } => {
                player.set_audio_with_metadata(data, duration, sample_rate, channels);
                Ok(())
            }
            Self::AudioFile {
                path,
                duration,
                sample_rate,
                channels,
            } => {
                player.set_audio_file_with_metadata(path, duration, sample_rate, channels);
                Ok(())
            }
            Self::WavFile { path } => player.set_wav_file(path),
            Self::AiffFile { path } => player.set_aiff_file(path),
            Self::DecodedSamples {
                audio_bytes,
                samples,
                duration,
                sample_rate,
                channels,
            } => {
                player.set_audio_samples_with_metadata(
                    audio_bytes,
                    samples,
                    duration,
                    sample_rate,
                    channels,
                );
                Ok(())
            }
            Self::PreparedTempo(tempo) => {
                player.set_prepared_tempo_samples(tempo.original_path, tempo.prepared);
                Ok(())
            }
            Self::InterleavedF32File {
                path,
                sample_count,
                duration,
                sample_rate,
                channels,
            } => {
                player.set_interleaved_f32_file_with_metadata(
                    path,
                    sample_count,
                    duration,
                    sample_rate,
                    channels,
                );
                Ok(())
            }
        }
    }
}

/// Span and loop behavior for a playback command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlaybackRuntimeMode {
    /// Play the requested normalized span once.
    OneShot { start: f64, end: f64 },
    /// Loop the requested normalized span, starting at a normalized offset.
    Looped { start: f64, end: f64, offset: f64 },
}

impl PlaybackRuntimeMode {
    fn start_player(
        self,
        player: &mut AudioPlayer,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<f32, String> {
        match self {
            Self::OneShot { start, end } => {
                player.play_range_with_metronome_and_replace_policy(
                    start,
                    end,
                    false,
                    replace_policy,
                    metronome,
                )?;
                Ok(start.clamp(0.0, 1.0) as f32)
            }
            Self::Looped { start, end, offset } => {
                player.play_looped_range_from_with_metronome_and_replace_policy(
                    start,
                    end,
                    offset,
                    replace_policy,
                    metronome,
                )?;
                Ok(offset.clamp(start.min(end), start.max(end)).clamp(0.0, 1.0) as f32)
            }
        }
    }
}

/// How a new runtime playback request handles the previous active source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackRuntimeReplacePolicy {
    /// Fade out the previous source before it naturally leaves the mixer.
    #[default]
    FadeOutPrevious,
    /// Clear the previous source immediately before appending the new source.
    ClearPrevious,
}

/// In-place span-bound update for already running playback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRuntimeSpanUpdate {
    pub start: f64,
    pub end: f64,
    pub offset: f64,
    pub seek_to_offset: bool,
    /// Seek to the new span start when dispatch-time progress is outside it.
    ///
    /// The concrete player resolves this policy against its frame-quantized
    /// target plan immediately before dispatch. A finite position inside the
    /// requested span is preserved; an outside, missing, or non-finite
    /// position starts at `start`.
    pub seek_if_outside: bool,
    pub looped: bool,
    pub playback_gain: f32,
    pub playback_gain_normalization: Option<PlaybackRuntimeGainNormalization>,
    pub metronome: Option<PlaybackMetronomeConfig>,
}

/// Runtime-owned gain normalization for one normalized playback span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRuntimeGainNormalization {
    pub start: f32,
    pub end: f32,
}

impl PlaybackRuntimeGainNormalization {
    pub fn new(start: f32, end: f32) -> Self {
        Self { start, end }
    }
}

/// Complete neutral playback-start request.
#[derive(Clone, Debug)]
pub struct PlaybackRuntimeRequest {
    pub source: PlaybackRuntimeSource,
    pub mode: PlaybackRuntimeMode,
    pub stream_policy: PlaybackRuntimeStreamPolicy,
    pub volume: f32,
    /// Whether playback boundaries use anti-click edge fades.
    pub anti_click_enabled: bool,
    /// Anti-click edge fade duration in milliseconds; invalid values use the 3 ms default.
    pub anti_click_fade_ms: f32,
    pub playback_gain: f32,
    pub playback_gain_normalization: Option<PlaybackRuntimeGainNormalization>,
    pub replace_policy: PlaybackRuntimeReplacePolicy,
    pub edit_fade: Option<EditFadeRange>,
    pub metronome: Option<PlaybackMetronomeConfig>,
}

/// Successful playback-start outcome.
#[derive(Clone, Debug)]
pub struct PlaybackRuntimeStarted {
    pub id: PlaybackRequestId,
    pub output: ResolvedOutput,
    pub playback_start: f32,
}

/// Playback state snapshot returned by the audio-control runtime.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackRuntimeProgress {
    pub active: bool,
    pub elapsed: Option<Duration>,
    pub looping: bool,
    pub progress: Option<f32>,
    pub error: Option<String>,
    /// Exact frame bounds of the currently configured playback plan.
    ///
    /// These values come from the frame-quantized playback plan and remain
    /// lossless when a normalized UI position cannot represent the source
    /// frame precisely.
    pub playback_span_frames: Option<(u64, u64)>,
    /// Frame denominator used by the currently configured playback plan.
    ///
    /// This is the plan's quantized denominator, which can differ from a
    /// source container's nominal frame count.
    pub track_total_frames: Option<u64>,
    /// Last successful live span-retarget command for this playback lifetime.
    ///
    /// This is present only with a valid active configured-span tuple. It is
    /// reset when a new play is attempted or playback is stopped, so a later
    /// progress poll can acknowledge a retarget without reimplementing the
    /// player's plan quantization.
    pub configured_span_request_id: Option<PlaybackRequestId>,
    /// Configured beat count for the active sample metronome.
    ///
    /// This is retained after a successful play or span retarget, including for
    /// lazy sources that cannot expose a live playback-span control. It is
    /// `None` when the metronome is disabled or playback is inactive.
    pub metronome_beat_count: Option<u16>,
    /// The last successfully started current play request, if one exists.
    ///
    /// The runtime retains this identity after natural completion; inspect [`Self::active`] to
    /// determine whether the identified play is still running.
    pub playback_request_id: Option<PlaybackRequestId>,
}

/// Reason a playback command was cancelled before execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackRuntimeCancellation {
    /// A newer queued play request replaced this one before it started.
    Superseded,
    /// A stop request cancelled this queued play request before it started.
    Stopped,
    /// The runtime was shut down before this queued play request started.
    Shutdown,
}

/// Events emitted by the playback command runtime.
#[derive(Clone, Debug)]
pub enum PlaybackRuntimeEvent {
    Started(PlaybackRuntimeStarted),
    Failed {
        id: PlaybackRequestId,
        error: String,
    },
    Cancelled {
        id: PlaybackRequestId,
        reason: PlaybackRuntimeCancellation,
    },
    Stopped {
        id: PlaybackRequestId,
    },
    Progress {
        id: PlaybackRequestId,
        progress: PlaybackRuntimeProgress,
    },
}

/// Non-blocking playback-command submission failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackRuntimeSubmitError {
    QueueFull,
    Closed,
}

/// Cloneable handle for submitting playback commands to the runtime thread.
pub struct PlaybackRuntimeHandle {
    commands: Arc<PlaybackRuntimeCommandQueue>,
    next_id: Arc<AtomicU64>,
    metronome_mix: MetronomeMixOverride,
}

impl Clone for PlaybackRuntimeHandle {
    fn clone(&self) -> Self {
        self.commands.retain_handle();
        Self {
            commands: Arc::clone(&self.commands),
            next_id: Arc::clone(&self.next_id),
            metronome_mix: self.metronome_mix.clone(),
        }
    }
}

impl Drop for PlaybackRuntimeHandle {
    fn drop(&mut self) {
        self.commands.release_handle();
    }
}

impl PlaybackRuntimeHandle {
    /// Submit a playback request without waiting for the runtime thread.
    pub fn try_play(
        &self,
        request: PlaybackRuntimeRequest,
    ) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::Play { id, request })
            .map(|()| id)
    }

    /// Submit a stop command without waiting for the runtime thread.
    pub fn try_stop(&self) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::Stop { id })
            .map(|()| id)
    }

    /// Cancel queued playback without stopping the currently audible source.
    pub fn try_cancel_pending_playback(
        &self,
    ) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::CancelPendingPlayback)
            .map(|()| id)
    }

    /// Submit a playback-progress snapshot request without waiting for the runtime thread.
    pub fn try_poll_progress(&self) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::PollProgress { id })
            .map(|()| id)
    }

    /// Submit a volume update for current and future playback without waiting for the runtime thread.
    pub fn try_set_volume(&self, volume: f32) -> Result<(), PlaybackRuntimeSubmitError> {
        self.submit(PlaybackRuntimeCommand::SetVolume { volume })
    }

    /// Set the click mix for current and future playback without queuing or seeking.
    ///
    /// The audio source reads the latest mix at frame boundaries. The update leaves each
    /// request's enabled state and beat grid intact.
    pub fn set_metronome_mix(&self, mix: PlaybackMetronomeMix) {
        self.metronome_mix.set(mix);
    }

    /// Submit a playback-gain update for current and future playback without waiting for the runtime thread.
    pub fn try_set_playback_gain(&self, gain: f32) -> Result<(), PlaybackRuntimeSubmitError> {
        self.try_set_playback_gain_with_normalization(gain, None)
    }

    /// Submit a non-blocking playback-gain update with runtime-owned normalization.
    pub fn try_set_playback_gain_with_normalization(
        &self,
        gain: f32,
        normalization: Option<PlaybackRuntimeGainNormalization>,
    ) -> Result<(), PlaybackRuntimeSubmitError> {
        self.submit(PlaybackRuntimeCommand::SetPlaybackGain {
            gain,
            normalization,
        })
    }

    /// Submit a precomputed playback-gain update for one specific play request.
    ///
    /// The runtime applies the scalar only when `expected_playback_id` is still the current
    /// successfully started play. A stale result, a result arriving after stop, or a result
    /// superseded by another play is silently discarded. This command never performs file I/O or
    /// a normalization prepass on the runtime control thread.
    /// `Ok(())` confirms queue admission only; the bounded queue may coalesce or evict the command,
    /// and no application acknowledgement event is emitted.
    pub fn try_set_playback_gain_for(
        &self,
        expected_playback_id: PlaybackRequestId,
        gain: f32,
    ) -> Result<(), PlaybackRuntimeSubmitError> {
        self.submit(PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id,
            gain,
        })
    }

    /// Submit an in-place span-retarget request without waiting for the runtime thread.
    pub fn try_retarget_span(
        &self,
        update: PlaybackRuntimeSpanUpdate,
    ) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::RetargetSpan {
            id,
            expected_playback_id: None,
            update,
        })
        .map(|()| id)
    }

    /// Submit a span-retarget request only while the expected play request is current.
    ///
    /// The runtime compares `expected_playback` with its last successfully started play at
    /// dispatch. A mismatch emits a best-effort [`PlaybackRuntimeEvent::Cancelled`] response
    /// with [`PlaybackRuntimeCancellation::Superseded`] and never calls the audio executor. A
    /// matching play that is inactive, or whose authoritative preflight snapshot reports an
    /// error, emits a best-effort [`PlaybackRuntimeEvent::Progress`] response without retargeting.
    /// For an active non-seeking update, a finite runtime dispatch-time position replaces the
    /// caller's offset; seeking updates retain their requested offset. `seek_if_outside` is
    /// resolved by the concrete player from its exact frame plan and source position. Responses
    /// are disposable runtime events, but a later active progress poll retains the successful
    /// retarget identity with its configured frame tuple.
    pub fn try_retarget_active_span(
        &self,
        expected_playback: PlaybackRequestId,
        update: PlaybackRuntimeSpanUpdate,
    ) -> Result<PlaybackRequestId, PlaybackRuntimeSubmitError> {
        let id = self.next_request_id();
        self.submit(PlaybackRuntimeCommand::RetargetSpan {
            id,
            expected_playback_id: Some(expected_playback),
            update,
        })
        .map(|()| id)
    }

    /// Request runtime shutdown without waiting for the runtime thread.
    pub fn try_shutdown(&self) -> Result<(), PlaybackRuntimeSubmitError> {
        self.submit(PlaybackRuntimeCommand::Shutdown)
    }

    fn next_request_id(&self) -> PlaybackRequestId {
        PlaybackRequestId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn submit(&self, command: PlaybackRuntimeCommand) -> Result<(), PlaybackRuntimeSubmitError> {
        self.commands.try_submit(command)
    }
}

/// Handle plus event receiver for a spawned playback command runtime.
pub struct PlaybackRuntime {
    pub handle: PlaybackRuntimeHandle,
    pub events: PlaybackRuntimeEventReceiver,
}

/// Error returned when the playback runtime event receiver is disconnected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackRuntimeEventRecvError;

/// Error returned when a playback runtime event is not available before a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackRuntimeEventRecvTimeoutError {
    Timeout,
    Disconnected,
}

/// Non-blocking receiver for bounded playback runtime events.
pub struct PlaybackRuntimeEventReceiver {
    queue: Arc<PlaybackRuntimeEventQueue>,
}

impl PlaybackRuntimeEventReceiver {
    fn new(queue: Arc<PlaybackRuntimeEventQueue>) -> Self {
        Self { queue }
    }

    /// Receive the next event, waiting only for an event or runtime shutdown.
    pub fn recv(&self) -> Result<PlaybackRuntimeEvent, PlaybackRuntimeEventRecvError> {
        self.queue.recv()
    }

    /// Receive the next event without waiting longer than `timeout`.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<PlaybackRuntimeEvent, PlaybackRuntimeEventRecvTimeoutError> {
        self.queue.recv_timeout(timeout)
    }

    /// Iterate over events currently available without waiting.
    pub fn try_iter(&self) -> PlaybackRuntimeEventIter<'_> {
        PlaybackRuntimeEventIter { receiver: self }
    }
}

impl Drop for PlaybackRuntimeEventReceiver {
    fn drop(&mut self) {
        self.queue.close_receiver();
    }
}

/// Iterator over currently available playback runtime events.
pub struct PlaybackRuntimeEventIter<'a> {
    receiver: &'a PlaybackRuntimeEventReceiver,
}

impl Iterator for PlaybackRuntimeEventIter<'_> {
    type Item = PlaybackRuntimeEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.queue.try_recv()
    }
}

impl PlaybackRuntime {
    /// Spawn a dedicated audio-control thread that owns `AudioPlayer`.
    pub fn spawn(
        player: AudioPlayer,
        config: PlaybackRuntimeConfig,
    ) -> Result<Self, std::io::Error> {
        let metronome_mix = player.metronome_mix_override.clone();
        spawn_executor_with_mix(
            AudioPlayerPlaybackExecutor {
                player,
                configured_metronome_beat_count: None,
                tempo: None,
            },
            config,
            metronome_mix,
        )
    }
}

enum PlaybackRuntimeCommand {
    Play {
        id: PlaybackRequestId,
        request: PlaybackRuntimeRequest,
    },
    Stop {
        id: PlaybackRequestId,
    },
    CancelPendingPlayback,
    RetargetSpan {
        id: PlaybackRequestId,
        expected_playback_id: Option<PlaybackRequestId>,
        update: PlaybackRuntimeSpanUpdate,
    },
    PollProgress {
        id: PlaybackRequestId,
    },
    SetVolume {
        volume: f32,
    },
    SetPlaybackGain {
        gain: f32,
        normalization: Option<PlaybackRuntimeGainNormalization>,
    },
    SetPlaybackGainFor {
        expected_playback_id: PlaybackRequestId,
        gain: f32,
    },
    Shutdown,
}

struct PlaybackRuntimeCommandQueue {
    capacity: usize,
    handles: AtomicU64,
    state: Mutex<PlaybackRuntimeCommandQueueState>,
    wake: Condvar,
}

struct PlaybackRuntimeCommandQueueState {
    commands: VecDeque<PlaybackRuntimeCommand>,
    cancelled: VecDeque<(PlaybackRequestId, PlaybackRuntimeCancellation)>,
    closed: bool,
}

impl PlaybackRuntimeCommandQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            handles: AtomicU64::new(1),
            state: Mutex::new(PlaybackRuntimeCommandQueueState {
                commands: VecDeque::new(),
                cancelled: VecDeque::new(),
                closed: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn retain_handle(&self) {
        self.handles.fetch_add(1, Ordering::Relaxed);
    }

    fn release_handle(&self) {
        if self.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.close();
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("playback command queue lock");
        state.closed = true;
        self.wake.notify_one();
    }

    fn try_submit(
        &self,
        command: PlaybackRuntimeCommand,
    ) -> Result<(), PlaybackRuntimeSubmitError> {
        let mut state = self.state.lock().expect("playback command queue lock");
        if state.closed {
            return Err(PlaybackRuntimeSubmitError::Closed);
        }

        let coalescing_index = match &command {
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id,
                ..
            } => find_scoped_playback_gain_coalescing_index(&state.commands, *expected_playback_id),
            _ => state
                .commands
                .iter()
                .position(|queued| same_coalescing_class(queued, &command)),
        };
        if let Some(index) = coalescing_index {
            if matches!(
                state.commands.get(index),
                Some(PlaybackRuntimeCommand::Play { .. })
            ) && !self.can_record_cancellations(&state, 1)
            {
                return Err(PlaybackRuntimeSubmitError::QueueFull);
            }
            if let (Some(queued_id), Some(incoming_id)) = (
                state.commands.get(index).and_then(span_retarget_id),
                span_retarget_id(&command),
            ) && incoming_id <= queued_id
            {
                return Ok(());
            }
            let replaced = std::mem::replace(
                state
                    .commands
                    .get_mut(index)
                    .expect("coalescing command index"),
                command,
            );
            self.record_cancellation(
                replaced,
                PlaybackRuntimeCancellation::Superseded,
                &mut state,
            );
            self.wake.notify_one();
            return Ok(());
        }

        if state.commands.len() >= self.capacity {
            let can_evict = matches!(&command, PlaybackRuntimeCommand::Play { .. })
                || matches!(
                    &command,
                    PlaybackRuntimeCommand::RetargetSpan { .. }
                        | PlaybackRuntimeCommand::Stop { .. }
                        | PlaybackRuntimeCommand::CancelPendingPlayback
                        | PlaybackRuntimeCommand::Shutdown
                );
            if can_evict {
                let eviction = match &command {
                    PlaybackRuntimeCommand::Play { .. } => {
                        find_disposable_command(&state.commands, true)
                    }
                    PlaybackRuntimeCommand::RetargetSpan { .. } => {
                        find_disposable_command(&state.commands, false)
                    }
                    PlaybackRuntimeCommand::Stop { .. }
                    | PlaybackRuntimeCommand::CancelPendingPlayback
                    | PlaybackRuntimeCommand::Shutdown => find_noncritical_command(
                        &state.commands,
                        self.can_record_critical_cancellations(&state, 1),
                    ),
                    _ => None,
                };
                if let Some(index) = eviction {
                    if matches!(
                        state.commands.get(index),
                        Some(PlaybackRuntimeCommand::Play { .. })
                    ) && !self.can_record_critical_cancellations(&state, 1)
                    {
                        return Err(PlaybackRuntimeSubmitError::QueueFull);
                    }
                    let evicted = state.commands.remove(index).expect("evicted command");
                    let reason = match &command {
                        PlaybackRuntimeCommand::Stop { .. }
                        | PlaybackRuntimeCommand::CancelPendingPlayback => {
                            PlaybackRuntimeCancellation::Stopped
                        }
                        PlaybackRuntimeCommand::Shutdown => PlaybackRuntimeCancellation::Shutdown,
                        _ => PlaybackRuntimeCancellation::Superseded,
                    };
                    self.record_cancellation(evicted, reason, &mut state);
                } else if matches!(&command, PlaybackRuntimeCommand::Shutdown) {
                    let play_count = state
                        .commands
                        .iter()
                        .filter(|command| matches!(command, PlaybackRuntimeCommand::Play { .. }))
                        .count();
                    if !self.can_record_critical_cancellations(&state, play_count) {
                        return Err(PlaybackRuntimeSubmitError::QueueFull);
                    }
                    while let Some(evicted) = state.commands.pop_front() {
                        self.record_cancellation(
                            evicted,
                            PlaybackRuntimeCancellation::Shutdown,
                            &mut state,
                        );
                    }
                } else {
                    return Err(PlaybackRuntimeSubmitError::QueueFull);
                }
            } else {
                return Err(PlaybackRuntimeSubmitError::QueueFull);
            }
        }

        state.commands.push_back(command);
        self.wake.notify_one();
        Ok(())
    }

    fn can_record_cancellations(
        &self,
        state: &PlaybackRuntimeCommandQueueState,
        additional: usize,
    ) -> bool {
        self.can_record_cancellations_with_capacity(state, additional, self.capacity)
    }

    fn can_record_critical_cancellations(
        &self,
        state: &PlaybackRuntimeCommandQueueState,
        additional: usize,
    ) -> bool {
        self.can_record_cancellations_with_capacity(
            state,
            additional,
            self.capacity.saturating_add(CRITICAL_CANCELLATION_RESERVE),
        )
    }

    fn can_record_cancellations_with_capacity(
        &self,
        state: &PlaybackRuntimeCommandQueueState,
        additional: usize,
        capacity: usize,
    ) -> bool {
        state.cancelled.len().saturating_add(additional) <= capacity
    }

    fn record_cancellation(
        &self,
        command: PlaybackRuntimeCommand,
        reason: PlaybackRuntimeCancellation,
        state: &mut PlaybackRuntimeCommandQueueState,
    ) {
        let PlaybackRuntimeCommand::Play { id, .. } = command else {
            return;
        };
        debug_assert!(
            state.cancelled.len() < self.capacity.saturating_add(CRITICAL_CANCELLATION_RESERVE)
        );
        state.cancelled.push_back((id, reason));
    }

    fn recv(&self) -> Option<PlaybackRuntimeCommand> {
        let mut state = self.state.lock().expect("playback command queue lock");
        loop {
            if let Some(command) = state.commands.pop_front() {
                return Some(command);
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).expect("playback command queue wait");
        }
    }

    fn try_recv(&self) -> Result<PlaybackRuntimeCommand, TryRecvError> {
        let mut state = self.state.lock().expect("playback command queue lock");
        state.commands.pop_front().ok_or_else(|| {
            if state.closed {
                TryRecvError::Disconnected
            } else {
                TryRecvError::Empty
            }
        })
    }

    #[cfg(test)]
    fn retained_len(&self) -> usize {
        self.state
            .lock()
            .expect("playback command queue lock")
            .commands
            .len()
    }

    fn take_cancellations(&self) -> Vec<(PlaybackRequestId, PlaybackRuntimeCancellation)> {
        let mut state = self.state.lock().expect("playback command queue lock");
        state.cancelled.drain(..).collect()
    }
}

fn same_coalescing_class(
    queued: &PlaybackRuntimeCommand,
    incoming: &PlaybackRuntimeCommand,
) -> bool {
    if let (
        PlaybackRuntimeCommand::RetargetSpan {
            expected_playback_id: queued_play,
            ..
        },
        PlaybackRuntimeCommand::RetargetSpan {
            expected_playback_id: incoming_play,
            ..
        },
    ) = (queued, incoming)
    {
        return queued_play == incoming_play;
    }
    if let (
        PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id: queued_play,
            ..
        },
        PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id: incoming_play,
            ..
        },
    ) = (queued, incoming)
    {
        return queued_play == incoming_play;
    }
    matches!(
        (queued, incoming),
        (
            PlaybackRuntimeCommand::Play { .. },
            PlaybackRuntimeCommand::Play { .. }
        ) | (
            PlaybackRuntimeCommand::PollProgress { .. },
            PlaybackRuntimeCommand::PollProgress { .. }
        ) | (
            PlaybackRuntimeCommand::SetVolume { .. },
            PlaybackRuntimeCommand::SetVolume { .. }
        ) | (
            PlaybackRuntimeCommand::SetPlaybackGain { .. },
            PlaybackRuntimeCommand::SetPlaybackGain { .. }
        ) | (
            PlaybackRuntimeCommand::Stop { .. },
            PlaybackRuntimeCommand::Stop { .. }
        ) | (
            PlaybackRuntimeCommand::CancelPendingPlayback,
            PlaybackRuntimeCommand::CancelPendingPlayback
        ) | (
            PlaybackRuntimeCommand::Shutdown,
            PlaybackRuntimeCommand::Shutdown
        )
    )
}

fn find_scoped_playback_gain_coalescing_index(
    commands: &VecDeque<PlaybackRuntimeCommand>,
    expected_playback_id: PlaybackRequestId,
) -> Option<usize> {
    matches!(
        commands.back(),
        Some(PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id: queued_id,
            ..
        }) if *queued_id == expected_playback_id
    )
    .then(|| commands.len() - 1)
}

fn find_disposable_command(
    commands: &VecDeque<PlaybackRuntimeCommand>,
    allow_retarget: bool,
) -> Option<usize> {
    commands.iter().position(|command| {
        matches!(
            command,
            PlaybackRuntimeCommand::PollProgress { .. }
                | PlaybackRuntimeCommand::SetVolume { .. }
                | PlaybackRuntimeCommand::SetPlaybackGain { .. }
                | PlaybackRuntimeCommand::SetPlaybackGainFor { .. }
        ) || (allow_retarget && matches!(command, PlaybackRuntimeCommand::RetargetSpan { .. }))
    })
}

fn find_noncritical_command(
    commands: &VecDeque<PlaybackRuntimeCommand>,
    can_cancel_play: bool,
) -> Option<usize> {
    commands
        .iter()
        .position(|command| {
            matches!(
                command,
                PlaybackRuntimeCommand::PollProgress { .. }
                    | PlaybackRuntimeCommand::RetargetSpan { .. }
                    | PlaybackRuntimeCommand::SetVolume { .. }
                    | PlaybackRuntimeCommand::SetPlaybackGain { .. }
                    | PlaybackRuntimeCommand::SetPlaybackGainFor { .. }
            )
        })
        .or_else(|| {
            if can_cancel_play {
                commands
                    .iter()
                    .position(|command| matches!(command, PlaybackRuntimeCommand::Play { .. }))
            } else {
                None
            }
        })
}

struct PlaybackRuntimeEventQueue {
    capacity: usize,
    state: Mutex<PlaybackRuntimeEventQueueState>,
    wake: Condvar,
}

struct PlaybackRuntimeEventQueueState {
    events: VecDeque<PlaybackRuntimeEvent>,
    sender_open: bool,
    receiver_open: bool,
}

impl PlaybackRuntimeEventQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(PlaybackRuntimeEventQueueState {
                events: VecDeque::new(),
                sender_open: true,
                receiver_open: true,
            }),
            wake: Condvar::new(),
        }
    }

    fn try_send(&self, event: PlaybackRuntimeEvent) -> bool {
        let mut state = self.state.lock().expect("playback event queue lock");
        if !state.receiver_open {
            return false;
        }
        if state.events.len() >= self.capacity {
            // Progress is disposable. Started/failed/stopped events are kept
            // ahead of progress and cancellation notifications, but the
            // admission path must remain non-blocking even if every retained
            // slot already contains a terminal event.
            if is_progress_event(&event) {
                return false;
            }
            if let Some(index) = state.events.iter().position(is_progress_event) {
                state.events.remove(index);
            } else if let Some(index) = state
                .events
                .iter()
                .position(|queued| matches!(queued, PlaybackRuntimeEvent::Cancelled { .. }))
            {
                state.events.remove(index);
            } else {
                return false;
            }
        }
        state.events.push_back(event);
        self.wake.notify_one();
        true
    }

    fn try_recv(&self) -> Option<PlaybackRuntimeEvent> {
        self.state
            .lock()
            .expect("playback event queue lock")
            .events
            .pop_front()
    }

    fn recv(&self) -> Result<PlaybackRuntimeEvent, PlaybackRuntimeEventRecvError> {
        let mut state = self.state.lock().expect("playback event queue lock");
        loop {
            if let Some(event) = state.events.pop_front() {
                return Ok(event);
            }
            if !state.sender_open {
                return Err(PlaybackRuntimeEventRecvError);
            }
            state = self.wake.wait(state).expect("playback event queue wait");
        }
    }

    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<PlaybackRuntimeEvent, PlaybackRuntimeEventRecvTimeoutError> {
        let started = std::time::Instant::now();
        let mut state = self.state.lock().expect("playback event queue lock");
        loop {
            if let Some(event) = state.events.pop_front() {
                return Ok(event);
            }
            if !state.sender_open {
                return Err(PlaybackRuntimeEventRecvTimeoutError::Disconnected);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(PlaybackRuntimeEventRecvTimeoutError::Timeout);
            }
            let (next_state, wait_result) = self
                .wake
                .wait_timeout(state, remaining)
                .expect("playback event queue wait");
            state = next_state;
            if wait_result.timed_out() {
                return Err(PlaybackRuntimeEventRecvTimeoutError::Timeout);
            }
        }
    }

    fn close_sender(&self) {
        let mut state = self.state.lock().expect("playback event queue lock");
        state.sender_open = false;
        self.wake.notify_all();
    }

    fn close_receiver(&self) {
        let mut state = self.state.lock().expect("playback event queue lock");
        state.receiver_open = false;
        state.events.clear();
        self.wake.notify_all();
    }
}

fn is_progress_event(event: &PlaybackRuntimeEvent) -> bool {
    matches!(event, PlaybackRuntimeEvent::Progress { .. })
}

trait PlaybackRuntimeExecutor: Send + 'static {
    fn play(
        &mut self,
        request: PlaybackRuntimeRequest,
    ) -> Result<PlaybackRuntimeStartedData, String>;
    fn stop(&mut self) -> Result<(), String>;
    fn retarget_span(&mut self, update: PlaybackRuntimeSpanUpdate) -> Result<f32, String>;
    fn set_volume(&mut self, volume: f32);
    fn set_playback_gain(
        &mut self,
        gain: f32,
        normalization: Option<PlaybackRuntimeGainNormalization>,
    );
    fn progress(&mut self) -> PlaybackRuntimeProgress;
}

struct PlaybackRuntimeStartedData {
    output: ResolvedOutput,
    playback_start: f32,
}

struct AudioPlayerPlaybackExecutor {
    player: AudioPlayer,
    // The runtime owns `player` and applies every play/retarget configuration,
    // so this records the exact metronome configuration that crossed that
    // successful control-thread boundary. AudioPlayer's public readback stays
    // focused on live span controls, which lazy sources intentionally lack.
    configured_metronome_beat_count: Option<u16>,
    tempo: Option<TempoPlaybackTimeline>,
}

impl PlaybackRuntimeExecutor for AudioPlayerPlaybackExecutor {
    fn play(
        &mut self,
        request: PlaybackRuntimeRequest,
    ) -> Result<PlaybackRuntimeStartedData, String> {
        // A replacement attempt must not report a metronome configuration from
        // the previous playback lifetime if setup fails.
        self.configured_metronome_beat_count = None;
        self.tempo = None;
        let tempo_start = if let PlaybackRuntimeSource::PreparedTempo(tempo) = &request.source {
            if request.edit_fade.is_some() || request.metronome.is_some() {
                return Err(String::from(
                    "Prepared tempo playback requires ordinary playback for edit previews or metronome",
                ));
            }
            let plan = tempo
                .playback_plan(request.mode)
                .map_err(|error| error.to_string())?;
            Some((tempo.timeline(), plan))
        } else {
            None
        };
        let configured_metronome_beat_count = request.metronome.map(|config| config.beat_count());
        self.player
            .set_anti_clip_settings(request.anti_click_enabled, request.anti_click_fade_ms);
        self.player.set_volume(request.volume);
        self.player.set_stream_policy(request.stream_policy);
        self.player
            .set_playback_gain(runtime_playback_gain_for_source(
                request.playback_gain,
                request.playback_gain_normalization,
                &request.source,
            ));
        let output = self.player.output_details().clone();
        request.source.apply_to_player(&mut self.player)?;
        self.player.set_edit_fade_state(request.edit_fade);
        let playback_start = if let Some((timeline, plan)) = tempo_start {
            self.player
                .play_prepared_tempo_span(plan, request.replace_policy)?;
            let start = timeline.source_progress(plan.seek_offset_frames());
            self.tempo = Some(timeline);
            start
        } else {
            request.mode.start_player(
                &mut self.player,
                request.replace_policy,
                request.metronome,
            )?
        };
        self.configured_metronome_beat_count = configured_metronome_beat_count;
        Ok(PlaybackRuntimeStartedData {
            output,
            playback_start,
        })
    }

    fn stop(&mut self) -> Result<(), String> {
        self.player.stop();
        self.configured_metronome_beat_count = None;
        self.tempo = None;
        Ok(())
    }

    fn retarget_span(&mut self, update: PlaybackRuntimeSpanUpdate) -> Result<f32, String> {
        if self.tempo.is_some() {
            return Err(String::from(
                "Prepared tempo playback requires a new span request",
            ));
        }
        let configured_metronome_beat_count = update.metronome.map(|config| config.beat_count());
        self.player
            .set_playback_gain(runtime_playback_gain_for_player(
                update.playback_gain,
                update.playback_gain_normalization,
                &self.player,
            ));
        if update.looped {
            self.player
                .retarget_looped_range_with_metronome_and_seek_policy(
                    update.start,
                    update.end,
                    update.offset,
                    update.seek_to_offset,
                    update.seek_if_outside,
                    update.metronome,
                )?;
        } else {
            self.player
                .retarget_one_shot_range_with_metronome_and_seek_policy(
                    update.start,
                    update.end,
                    update.offset,
                    update.seek_to_offset,
                    update.seek_if_outside,
                    update.metronome,
                )?;
        }
        self.configured_metronome_beat_count = configured_metronome_beat_count;
        Ok(update
            .offset
            .clamp(update.start.min(update.end), update.start.max(update.end))
            .clamp(0.0, 1.0) as f32)
    }

    fn set_volume(&mut self, volume: f32) {
        self.player.set_volume(volume);
    }

    fn set_playback_gain(
        &mut self,
        gain: f32,
        normalization: Option<PlaybackRuntimeGainNormalization>,
    ) {
        let gain = if let Some(timeline) = &self.tempo {
            let normalized = normalization
                .zip(self.player.playback_samples.as_ref())
                .and_then(|(normalization, samples)| {
                    timeline.normalized_gain(
                        samples,
                        usize::from(self.player.track_channels?),
                        normalization,
                    )
                });
            sanitize_playback_gain(gain * normalized.unwrap_or(1.0))
        } else {
            runtime_playback_gain_for_player(gain, normalization, &self.player)
        };
        self.player.set_playback_gain(gain);
    }

    fn progress(&mut self) -> PlaybackRuntimeProgress {
        let active = self.player.is_playing();
        let (playback_span_frames, track_total_frames, metronome_beat_count) = if active {
            (
                self.player.playback_span_frames(),
                self.player.track_total_frames(),
                self.configured_metronome_beat_count,
            )
        } else {
            self.configured_metronome_beat_count = None;
            (None, None, None)
        };
        let mut progress = PlaybackRuntimeProgress {
            active,
            elapsed: self.player.playback_elapsed(),
            looping: self.player.is_looping(),
            progress: self.player.progress(),
            error: self.player.take_error(),
            playback_span_frames,
            track_total_frames,
            configured_span_request_id: None,
            metronome_beat_count,
            playback_request_id: None,
        };
        if let Some(timeline) = &self.tempo {
            timeline.apply_progress(&mut progress, self.player.progress_frame());
        }
        progress
    }
}

fn authoritative_progress(
    executor: &mut impl PlaybackRuntimeExecutor,
    playback_request_id: Option<PlaybackRequestId>,
    configured_span_request_id: Option<PlaybackRequestId>,
) -> PlaybackRuntimeProgress {
    let mut progress = executor.progress();
    progress.playback_request_id = playback_request_id;
    normalize_configured_span_metadata(&mut progress, configured_span_request_id);
    progress
}

fn normalize_configured_span_metadata(
    progress: &mut PlaybackRuntimeProgress,
    configured_span_request_id: Option<PlaybackRequestId>,
) {
    let configured_span = match (
        progress.active,
        progress.playback_request_id,
        progress.playback_span_frames,
        progress.track_total_frames,
    ) {
        (true, Some(_), Some((start, end)), Some(total)) if start < end && end <= total => {
            Some(((start, end), total))
        }
        _ => None,
    };

    if let Some((span, total)) = configured_span {
        progress.playback_span_frames = Some(span);
        progress.track_total_frames = Some(total);
        progress.configured_span_request_id = configured_span_request_id;
    } else {
        progress.playback_span_frames = None;
        progress.track_total_frames = None;
        progress.configured_span_request_id = None;
        progress.metronome_beat_count = None;
    }
}

fn apply_dispatch_time_progress(
    update: &mut PlaybackRuntimeSpanUpdate,
    progress: &PlaybackRuntimeProgress,
) {
    // `seek_if_outside` is resolved after the AudioPlayer has quantized the
    // target span to source frames. Keeping it untouched here avoids losing
    // adjacent-frame distinctions in the public f32 progress ratio.
    if update.seek_to_offset || update.seek_if_outside {
        return;
    }
    if let Some(progress_ratio) = progress.progress.filter(|ratio| ratio.is_finite()) {
        update.offset = f64::from(progress_ratio);
    }
}

#[cfg(test)]
fn spawn_executor(
    executor: impl PlaybackRuntimeExecutor,
    config: PlaybackRuntimeConfig,
) -> Result<PlaybackRuntime, std::io::Error> {
    spawn_executor_with_mix(executor, config, MetronomeMixOverride::default())
}

fn spawn_executor_with_mix(
    executor: impl PlaybackRuntimeExecutor,
    config: PlaybackRuntimeConfig,
    metronome_mix: MetronomeMixOverride,
) -> Result<PlaybackRuntime, std::io::Error> {
    let queue_capacity = config.queue_capacity.max(1);
    let command_queue = Arc::new(PlaybackRuntimeCommandQueue::new(queue_capacity));
    let event_queue = Arc::new(PlaybackRuntimeEventQueue::new(queue_capacity));
    let handle = PlaybackRuntimeHandle {
        commands: Arc::clone(&command_queue),
        next_id: Arc::new(AtomicU64::new(1)),
        metronome_mix,
    };
    let runtime_event_queue = Arc::clone(&event_queue);
    thread::Builder::new()
        .name(String::from("reson-playback-runtime"))
        .spawn(move || run_playback_runtime(executor, command_queue, runtime_event_queue))?;
    Ok(PlaybackRuntime {
        handle,
        events: PlaybackRuntimeEventReceiver::new(event_queue),
    })
}

fn run_playback_runtime(
    mut executor: impl PlaybackRuntimeExecutor,
    commands: Arc<PlaybackRuntimeCommandQueue>,
    events: Arc<PlaybackRuntimeEventQueue>,
) {
    let mut pending = VecDeque::new();
    let mut latest_span_retarget_id = None;
    let mut current_playback_id = None;
    let mut configured_span_request_id = None;
    while let Some(command) = next_runtime_command(&commands, &mut pending) {
        match coalesce_command(command, &commands, &events, &mut pending) {
            CoalescedCommand::Play { id, request } => {
                configured_span_request_id = None;
                let event = match executor.play(request) {
                    Ok(started) => {
                        current_playback_id = Some(id);
                        PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted {
                            id,
                            output: started.output,
                            playback_start: started.playback_start,
                        })
                    }
                    Err(error) => {
                        current_playback_id = None;
                        PlaybackRuntimeEvent::Failed { id, error }
                    }
                };
                send_runtime_event(&events, event);
            }
            CoalescedCommand::Stop { id } => {
                configured_span_request_id = None;
                let result = executor.stop();
                current_playback_id = None;
                let event = match result {
                    Ok(()) => PlaybackRuntimeEvent::Stopped { id },
                    Err(error) => PlaybackRuntimeEvent::Failed { id, error },
                };
                send_runtime_event(&events, event);
            }
            CoalescedCommand::CancelPendingPlayback => {}
            CoalescedCommand::RetargetSpan {
                id,
                expected_playback_id,
                mut update,
            } => {
                if expected_playback_id.is_some() && expected_playback_id != current_playback_id {
                    send_cancelled(id, PlaybackRuntimeCancellation::Superseded, &events);
                    continue;
                }
                if latest_span_retarget_id.is_some_and(|latest| id <= latest) {
                    continue;
                }
                if expected_playback_id.is_none() {
                    latest_span_retarget_id = Some(id);
                }

                if expected_playback_id.is_some() {
                    let progress = authoritative_progress(
                        &mut executor,
                        current_playback_id,
                        configured_span_request_id,
                    );
                    if !progress.active || progress.error.is_some() {
                        send_runtime_event(
                            &events,
                            PlaybackRuntimeEvent::Progress { id, progress },
                        );
                        continue;
                    }
                    latest_span_retarget_id = Some(id);
                    apply_dispatch_time_progress(&mut update, &progress);
                }

                let event = match executor.retarget_span(update) {
                    Ok(_) => {
                        configured_span_request_id = Some(id);
                        PlaybackRuntimeEvent::Progress {
                            id,
                            progress: authoritative_progress(
                                &mut executor,
                                current_playback_id,
                                configured_span_request_id,
                            ),
                        }
                    }
                    Err(error) => PlaybackRuntimeEvent::Failed { id, error },
                };
                send_runtime_event(&events, event);
            }
            CoalescedCommand::PollProgress { id } => {
                send_runtime_event(
                    &events,
                    PlaybackRuntimeEvent::Progress {
                        id,
                        progress: authoritative_progress(
                            &mut executor,
                            current_playback_id,
                            configured_span_request_id,
                        ),
                    },
                );
            }
            CoalescedCommand::SetVolume { volume } => {
                executor.set_volume(volume);
            }
            CoalescedCommand::SetPlaybackGain {
                gain,
                normalization,
            } => {
                executor.set_playback_gain(gain, normalization);
            }
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id,
                gain,
            } => {
                if current_playback_id == Some(expected_playback_id) {
                    executor.set_playback_gain(gain, None);
                }
            }
            CoalescedCommand::Shutdown => {
                let _ = current_playback_id.take();
                commands.close();
                break;
            }
        }
    }
    // The event receiver is the browser's teardown barrier. Drop the executor
    // first so its AudioPlayer releases the output device before the receiver
    // reports that the runtime has disconnected.
    drop(executor);
    events.close_sender();
}

fn next_runtime_command(
    commands: &PlaybackRuntimeCommandQueue,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> Option<PlaybackRuntimeCommand> {
    pending.pop_front().or_else(|| commands.recv())
}

enum CoalescedCommand {
    Play {
        id: PlaybackRequestId,
        request: PlaybackRuntimeRequest,
    },
    Stop {
        id: PlaybackRequestId,
    },
    CancelPendingPlayback,
    RetargetSpan {
        id: PlaybackRequestId,
        expected_playback_id: Option<PlaybackRequestId>,
        update: PlaybackRuntimeSpanUpdate,
    },
    PollProgress {
        id: PlaybackRequestId,
    },
    SetVolume {
        volume: f32,
    },
    SetPlaybackGain {
        gain: f32,
        normalization: Option<PlaybackRuntimeGainNormalization>,
    },
    SetPlaybackGainFor {
        expected_playback_id: PlaybackRequestId,
        gain: f32,
    },
    Shutdown,
}

fn coalesce_command(
    command: PlaybackRuntimeCommand,
    commands: &PlaybackRuntimeCommandQueue,
    events: &PlaybackRuntimeEventQueue,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> CoalescedCommand {
    for (id, reason) in commands.take_cancellations() {
        send_cancelled(id, reason, events);
    }
    loop {
        if pending.len() >= commands.capacity {
            return command_to_coalesced(coalesce_pending_command(command, pending, events));
        }
        match commands.try_recv() {
            Ok(next) => pending.push_back(next),
            Err(TryRecvError::Empty) => {
                return command_to_coalesced(coalesce_pending_command(command, pending, events));
            }
            Err(TryRecvError::Disconnected) => {
                cancel_command(command, PlaybackRuntimeCancellation::Shutdown, events);
                cancel_pending_commands(pending, PlaybackRuntimeCancellation::Shutdown, events);
                return CoalescedCommand::Shutdown;
            }
        }
    }
}

fn coalesce_pending_command(
    command: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
    events: &PlaybackRuntimeEventQueue,
) -> PlaybackRuntimeCommand {
    match command {
        current @ PlaybackRuntimeCommand::Play { .. } => {
            coalesce_play_command(current, pending, events)
        }
        current @ PlaybackRuntimeCommand::PollProgress { .. } => {
            coalesce_repeated_progress_command(current, pending)
        }
        current @ PlaybackRuntimeCommand::RetargetSpan { .. } => {
            coalesce_span_retarget_command(current, pending)
        }
        current @ PlaybackRuntimeCommand::SetVolume { .. } => {
            coalesce_repeated_volume_command(current, pending)
        }
        current @ PlaybackRuntimeCommand::SetPlaybackGain { .. } => {
            coalesce_repeated_playback_gain_command(current, pending)
        }
        current @ PlaybackRuntimeCommand::SetPlaybackGainFor { .. } => {
            coalesce_repeated_scoped_playback_gain_command(current, pending)
        }
        PlaybackRuntimeCommand::Shutdown => {
            cancel_pending_commands(pending, PlaybackRuntimeCancellation::Shutdown, events);
            PlaybackRuntimeCommand::Shutdown
        }
        current @ PlaybackRuntimeCommand::CancelPendingPlayback => {
            cancel_pending_play_commands(pending, events);
            current
        }
        // Stop is an ordering barrier: dropping it lets an old loop keep running
        // while the host app is still loading the replacement sample.
        current @ PlaybackRuntimeCommand::Stop { .. } => current,
    }
}

fn coalesce_play_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
    events: &PlaybackRuntimeEventQueue,
) -> PlaybackRuntimeCommand {
    loop {
        match pending.front() {
            Some(PlaybackRuntimeCommand::Play { .. }) => {
                let next = pending.pop_front().expect("pending play command");
                cancel_command(current, PlaybackRuntimeCancellation::Superseded, events);
                current = next;
            }
            Some(PlaybackRuntimeCommand::Stop { .. }) => {
                let stop = pending.pop_front().expect("pending stop command");
                cancel_command(current, PlaybackRuntimeCancellation::Stopped, events);
                return stop;
            }
            Some(PlaybackRuntimeCommand::CancelPendingPlayback) => {
                let cancel = pending
                    .pop_front()
                    .expect("pending cancel-playback command");
                cancel_command(current, PlaybackRuntimeCancellation::Stopped, events);
                return cancel;
            }
            Some(PlaybackRuntimeCommand::Shutdown) => {
                let shutdown = pending.pop_front().expect("pending shutdown command");
                cancel_command(current, PlaybackRuntimeCancellation::Shutdown, events);
                cancel_pending_commands(pending, PlaybackRuntimeCancellation::Shutdown, events);
                return shutdown;
            }
            Some(PlaybackRuntimeCommand::PollProgress { .. }) => {
                let _ = pending.pop_front();
            }
            Some(PlaybackRuntimeCommand::RetargetSpan { .. })
            | Some(PlaybackRuntimeCommand::SetVolume { .. })
            | Some(PlaybackRuntimeCommand::SetPlaybackGain { .. })
            | Some(PlaybackRuntimeCommand::SetPlaybackGainFor { .. })
            | None => return current,
        }
    }
}

fn coalesce_span_retarget_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> PlaybackRuntimeCommand {
    loop {
        match pending.front() {
            Some(next @ PlaybackRuntimeCommand::RetargetSpan { .. }) => {
                if !same_coalescing_class(&current, next) {
                    return current;
                }
                let next = pending.pop_front().expect("pending span retarget");
                let current_id = span_retarget_id(&current).expect("current span retarget");
                let next_id = span_retarget_id(&next).expect("next span retarget");
                if next_id > current_id {
                    current = next;
                }
            }
            Some(PlaybackRuntimeCommand::Play { .. }) => {
                return pending.pop_front().expect("pending play command");
            }
            Some(PlaybackRuntimeCommand::Stop { .. }) => {
                return pending.pop_front().expect("pending stop command");
            }
            Some(PlaybackRuntimeCommand::CancelPendingPlayback) => {
                return pending
                    .pop_front()
                    .expect("pending cancel-playback command");
            }
            Some(PlaybackRuntimeCommand::Shutdown) => {
                return pending.pop_front().expect("pending shutdown command");
            }
            Some(PlaybackRuntimeCommand::PollProgress { .. }) => {
                let _ = pending.pop_front();
            }
            Some(PlaybackRuntimeCommand::SetVolume { .. }) | None => return current,
            Some(PlaybackRuntimeCommand::SetPlaybackGain { .. })
            | Some(PlaybackRuntimeCommand::SetPlaybackGainFor { .. }) => return current,
        }
    }
}

fn span_retarget_id(command: &PlaybackRuntimeCommand) -> Option<PlaybackRequestId> {
    match command {
        PlaybackRuntimeCommand::RetargetSpan { id, .. } => Some(*id),
        _ => None,
    }
}

fn coalesce_repeated_progress_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> PlaybackRuntimeCommand {
    while matches!(
        pending.front(),
        Some(PlaybackRuntimeCommand::PollProgress { .. })
    ) {
        current = pending.pop_front().expect("pending progress command");
    }
    current
}

fn coalesce_repeated_volume_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> PlaybackRuntimeCommand {
    while matches!(
        pending.front(),
        Some(PlaybackRuntimeCommand::SetVolume { .. })
    ) {
        current = pending.pop_front().expect("pending volume command");
    }
    current
}

fn coalesce_repeated_playback_gain_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> PlaybackRuntimeCommand {
    while matches!(
        pending.front(),
        Some(PlaybackRuntimeCommand::SetPlaybackGain { .. })
    ) {
        current = pending.pop_front().expect("pending playback gain command");
    }
    current
}

fn coalesce_repeated_scoped_playback_gain_command(
    mut current: PlaybackRuntimeCommand,
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
) -> PlaybackRuntimeCommand {
    while let Some(next) = pending.front() {
        if !same_coalescing_class(&current, next) {
            break;
        }
        current = pending
            .pop_front()
            .expect("pending scoped playback gain command");
    }
    current
}

fn command_to_coalesced(command: PlaybackRuntimeCommand) -> CoalescedCommand {
    match command {
        PlaybackRuntimeCommand::Play { id, request } => CoalescedCommand::Play { id, request },
        PlaybackRuntimeCommand::Stop { id } => CoalescedCommand::Stop { id },
        PlaybackRuntimeCommand::CancelPendingPlayback => CoalescedCommand::CancelPendingPlayback,
        PlaybackRuntimeCommand::RetargetSpan {
            id,
            expected_playback_id,
            update,
        } => CoalescedCommand::RetargetSpan {
            id,
            expected_playback_id,
            update,
        },
        PlaybackRuntimeCommand::PollProgress { id } => CoalescedCommand::PollProgress { id },
        PlaybackRuntimeCommand::SetVolume { volume } => CoalescedCommand::SetVolume { volume },
        PlaybackRuntimeCommand::SetPlaybackGain {
            gain,
            normalization,
        } => CoalescedCommand::SetPlaybackGain {
            gain,
            normalization,
        },
        PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id,
            gain,
        } => CoalescedCommand::SetPlaybackGainFor {
            expected_playback_id,
            gain,
        },
        PlaybackRuntimeCommand::Shutdown => CoalescedCommand::Shutdown,
    }
}

fn cancel_command(
    command: PlaybackRuntimeCommand,
    reason: PlaybackRuntimeCancellation,
    events: &PlaybackRuntimeEventQueue,
) {
    if let PlaybackRuntimeCommand::Play { id, .. } = command {
        send_cancelled(id, reason, events);
    }
}

fn cancel_pending_commands(
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
    reason: PlaybackRuntimeCancellation,
    events: &PlaybackRuntimeEventQueue,
) {
    while let Some(command) = pending.pop_front() {
        cancel_command(command, reason, events);
    }
}

fn cancel_pending_play_commands(
    pending: &mut VecDeque<PlaybackRuntimeCommand>,
    events: &PlaybackRuntimeEventQueue,
) {
    let mut retained = VecDeque::new();
    while let Some(command) = pending.pop_front() {
        match command {
            PlaybackRuntimeCommand::Play { id, .. } => {
                send_cancelled(id, PlaybackRuntimeCancellation::Stopped, events);
            }
            other => retained.push_back(other),
        }
    }
    *pending = retained;
}

fn send_cancelled(
    id: PlaybackRequestId,
    reason: PlaybackRuntimeCancellation,
    events: &PlaybackRuntimeEventQueue,
) {
    send_runtime_event(events, PlaybackRuntimeEvent::Cancelled { id, reason });
}

fn send_runtime_event(events: &PlaybackRuntimeEventQueue, event: PlaybackRuntimeEvent) {
    let _ = events.try_send(event);
}

fn runtime_playback_gain_for_source(
    base_gain: f32,
    normalization: Option<PlaybackRuntimeGainNormalization>,
    source: &PlaybackRuntimeSource,
) -> f32 {
    let Some(normalization) = normalization else {
        return sanitize_playback_gain(base_gain);
    };
    let normalized = normalized_gain_for_runtime_source(source, normalization).unwrap_or(1.0);
    sanitize_playback_gain(base_gain * normalized)
}

fn runtime_playback_gain_for_player(
    base_gain: f32,
    normalization: Option<PlaybackRuntimeGainNormalization>,
    player: &AudioPlayer,
) -> f32 {
    let Some(normalization) = normalization else {
        return sanitize_playback_gain(base_gain);
    };
    let normalized = normalized_gain_for_player(player, normalization).unwrap_or(1.0);
    sanitize_playback_gain(base_gain * normalized)
}

fn sanitize_playback_gain(gain: f32) -> f32 {
    if gain.is_finite() && gain > 0.0 {
        gain
    } else {
        1.0
    }
}

fn normalized_gain_for_runtime_source(
    source: &PlaybackRuntimeSource,
    normalization: PlaybackRuntimeGainNormalization,
) -> Option<f32> {
    match source {
        PlaybackRuntimeSource::DecodedSamples {
            samples, channels, ..
        } => normalized_gain_for_interleaved_span(
            samples,
            *channels,
            normalization.start,
            normalization.end,
        ),
        PlaybackRuntimeSource::PreparedTempo(tempo) => tempo.timeline().normalized_gain(
            &tempo.prepared.samples,
            tempo.prepared.channels,
            normalization,
        ),
        PlaybackRuntimeSource::InterleavedF32File {
            path,
            sample_count,
            channels,
            ..
        } => normalized_gain_for_interleaved_f32_file_span(
            path,
            (*sample_count).try_into().ok()?,
            *channels,
            normalization.start,
            normalization.end,
        ),
        PlaybackRuntimeSource::AudioBytes { .. }
        | PlaybackRuntimeSource::AudioFile { .. }
        | PlaybackRuntimeSource::AiffFile { .. }
        | PlaybackRuntimeSource::WavFile { .. } => None,
    }
}

fn normalized_gain_for_player(
    player: &AudioPlayer,
    normalization: PlaybackRuntimeGainNormalization,
) -> Option<f32> {
    let channels = usize::from(player.track_channels.unwrap_or(1)).max(1);
    if let Some(samples) = player.playback_samples.as_ref() {
        return normalized_gain_for_interleaved_span(
            samples,
            channels,
            normalization.start,
            normalization.end,
        );
    }
    match player.current_audio.as_ref()? {
        super::AudioPlaybackSource::InterleavedF32File { path, sample_count } => {
            normalized_gain_for_interleaved_f32_file_span(
                path,
                (*sample_count).try_into().ok()?,
                channels,
                normalization.start,
                normalization.end,
            )
        }
        super::AudioPlaybackSource::Bytes(_) | super::AudioPlaybackSource::File(_) => None,
    }
}

fn normalized_gain_for_interleaved_span(
    samples: &[f32],
    channels: usize,
    start: f32,
    end: f32,
) -> Option<f32> {
    let bounds = interleaved_span_sample_bounds(samples.len(), channels, start, end)?;
    let peak = samples[bounds]
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    Some(normalized_gain_from_peak(peak))
}

fn normalized_gain_for_interleaved_f32_file_span(
    path: &Path,
    sample_count: usize,
    channels: usize,
    start: f32,
    end: f32,
) -> Option<f32> {
    let bounds = interleaved_span_sample_bounds(sample_count, channels, start, end)?;
    let mut reader = BufReader::new(File::open(path).ok()?);
    let byte_offset = bounds.start.checked_mul(F32_SAMPLE_BYTES)? as u64;
    reader.seek(SeekFrom::Start(byte_offset)).ok()?;

    let mut remaining = bounds.end.saturating_sub(bounds.start);
    let mut bytes = vec![0_u8; NORMALIZED_GAIN_READ_SAMPLES * F32_SAMPLE_BYTES];
    let mut peak = 0.0_f32;
    while remaining > 0 {
        let samples_to_read = remaining.min(NORMALIZED_GAIN_READ_SAMPLES);
        let byte_len = samples_to_read * F32_SAMPLE_BYTES;
        reader.read_exact(&mut bytes[..byte_len]).ok()?;
        for sample in bytes[..byte_len].chunks_exact(F32_SAMPLE_BYTES) {
            peak = peak.max(f32::from_le_bytes(sample.try_into().ok()?).abs());
        }
        remaining -= samples_to_read;
    }
    Some(normalized_gain_from_peak(peak))
}

fn interleaved_span_sample_bounds(
    sample_count: usize,
    channels: usize,
    start: f32,
    end: f32,
) -> Option<Range<usize>> {
    if sample_count == 0 || !start.is_finite() || !end.is_finite() {
        return None;
    }
    let channels = channels.max(1);
    let total_frames = sample_count / channels;
    if total_frames == 0 {
        return None;
    }
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let start_frame = (start.clamp(0.0, 1.0) * total_frames as f32).floor() as usize;
    let mut end_frame = (end.clamp(0.0, 1.0) * total_frames as f32).ceil() as usize;
    if end_frame <= start_frame {
        end_frame = (start_frame + 1).min(total_frames);
    }
    let start_idx = start_frame.saturating_mul(channels);
    let end_idx = end_frame.saturating_mul(channels).min(sample_count);
    (start_idx < end_idx).then_some(start_idx..end_idx)
}

fn normalized_gain_from_peak(peak: f32) -> f32 {
    if peak.is_finite() && peak > f32::EPSILON {
        1.0 / peak
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{
        PlaybackChannelLayout, PlaybackSeekBehavior, PlaybackSourceIdentity, PlaybackSourceKind,
        PlaybackSpanPlan, PlaybackSpanRequest,
    };
    use std::io::Write;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[cfg(all(target_os = "macos", feature = "native-audio"))]
    #[test]
    #[ignore = "requires explicit native CoreAudio output permission"]
    fn macos_real_executor_applies_anti_click_settings_independently_of_edit_fades() {
        use cpal::traits::{DeviceTrait, HostTrait};
        use std::time::Instant;

        assert_eq!(
            std::env::var("YIELD_TEST_AUDIO_OUTPUT").ok().as_deref(),
            Some("1"),
            "set YIELD_TEST_AUDIO_OUTPUT=1 to explicitly enable real audio output"
        );

        let host_id = cpal::available_hosts()
            .into_iter()
            .find(|host| host.name().eq_ignore_ascii_case("coreaudio"))
            .expect("CoreAudio host is available");
        let host = cpal::host_from_id(host_id.clone()).expect("open CoreAudio host");
        let default_device = host
            .default_output_device()
            .expect("CoreAudio default output device");
        let default_uid = default_device
            .id()
            .expect("read default output device identity")
            .1;
        let device_name =
            crate::device::device_label(&default_device).expect("read default output name");
        let (matching_name_count, default_uid_matches_name) = host
            .output_devices()
            .expect("enumerate CoreAudio outputs")
            .fold((0, false), |(count, uid_matches), device| {
                if crate::device::device_label(&device).is_some_and(|name| name == device_name) {
                    (
                        count + 1,
                        uid_matches
                            || device
                                .id()
                                .is_ok_and(|device_id| device_id.1 == default_uid),
                    )
                } else {
                    (count, uid_matches)
                }
            });
        assert_eq!(
            matching_name_count, 1,
            "the exact default device name must resolve uniquely"
        );
        assert!(
            default_uid_matches_name,
            "the uniquely resolved device name must belong to the current default output"
        );
        let sample_rate = default_device
            .default_output_config()
            .expect("read default output format")
            .sample_rate();

        let config = crate::output::AudioOutputConfig {
            host: Some(host_id.name().to_owned()),
            device: Some(device_name.clone()),
            sample_rate: Some(sample_rate),
            buffer_size: None,
        };
        let player = AudioPlayer::from_config(&config).expect("open exact CoreAudio output");
        let resolved = player.output_details();
        assert_eq!(resolved.host_id, host_id.name());
        assert_eq!(resolved.device_name, device_name);
        assert_eq!(resolved.sample_rate, sample_rate);
        assert!(resolved.channel_count >= 2);
        assert!(!resolved.used_fallback);
        let mut executor = AudioPlayerPlaybackExecutor {
            player,
            configured_metronome_beat_count: None,
            tempo: None,
        };

        let root = tempfile::tempdir().expect("temporary WAV root");
        let path = root.path().join("anti-click-native.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create tiny WAV");
        for _ in 0..4_096 {
            writer.write_sample(1_000_i16).expect("write left sample");
            writer.write_sample(-1_000_i16).expect("write right sample");
        }
        writer.finalize().expect("finalize tiny WAV");

        let edit_fade = EditFadeRange::new(
            0.25,
            0.75,
            0.4,
            Some(crate::player::FadeParams::new(0.2, 0.5, 0.0)),
            Some(crate::player::FadeParams::new(0.3, 0.5, 0.0)),
        );
        let request = |anti_click_enabled, anti_click_fade_ms| PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile { path: path.clone() },
            mode: PlaybackRuntimeMode::Looped {
                start: 0.0,
                end: 1.0,
                offset: 0.0,
            },
            stream_policy: PlaybackRuntimeStreamPolicy::full(),
            // Keep a low but measurable native signal so allocator assertions
            // cover callbacks that actually convert and emit source samples.
            volume: 0.1,
            anti_click_enabled,
            anti_click_fade_ms,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: Some(edit_fade),
            metronome: None,
        };
        let teardown_complete = executor.player.native_stream_teardown_complete();
        let mut previous_callback_samples = executor.player.native_callback_sample_count(0);

        for (index, (enabled, fade_ms, expected_fade)) in [
            (true, 2.0, Duration::from_millis(2)),
            (false, 0.0, Duration::ZERO),
            (true, 3.0, Duration::from_millis(3)),
        ]
        .into_iter()
        .enumerate()
        {
            executor
                .play(request(enabled, fade_ms))
                .expect("production executor starts native WAV playback");
            let callback_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < callback_deadline
                && (executor.player.native_active_source_count() == 0
                    || executor.player.native_callback_sample_count(0) <= previous_callback_samples)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let callback_samples = executor.player.native_callback_sample_count(0);
            assert!(executor.player.native_active_source_count() > 0);
            assert!(callback_samples > previous_callback_samples);
            assert!(executor.player.native_callback_peak(0) > 0.001);
            assert!(executor.player.native_callback_peak(1) > 0.001);
            previous_callback_samples = callback_samples;
            assert_eq!(executor.player.anti_clip_enabled, enabled);
            assert_eq!(executor.player.anti_clip_fade, expected_fade);
            let edit_snapshot = executor.player.edit_fade_handle.snapshot();
            assert!(edit_snapshot.active);
            let duration = 4_096.0 / f64::from(sample_rate) as f32;
            assert!((edit_snapshot.start_seconds - duration * 0.25).abs() < 1.0e-6);
            assert!((edit_snapshot.end_seconds - duration * 0.75).abs() < 1.0e-6);
            assert!((edit_snapshot.gain - 0.4).abs() < f32::EPSILON);
            assert_eq!(edit_snapshot.fade_in, edit_fade.fade_in);
            assert_eq!(edit_snapshot.fade_out, edit_fade.fade_out);
            if index == 0 {
                executor.player.set_anti_clip_settings(false, 0.0);
                let after_anti_click_change = executor.player.edit_fade_handle.snapshot();
                assert_eq!(after_anti_click_change.active, edit_snapshot.active);
                assert_eq!(
                    after_anti_click_change.start_seconds,
                    edit_snapshot.start_seconds
                );
                assert_eq!(
                    after_anti_click_change.end_seconds,
                    edit_snapshot.end_seconds
                );
                assert_eq!(after_anti_click_change.gain, edit_snapshot.gain);
                assert_eq!(after_anti_click_change.fade_in, edit_snapshot.fade_in);
                assert_eq!(after_anti_click_change.fade_out, edit_snapshot.fade_out);
            }
        }
        // Reuse the real output stream for malformed IEEE-float WAV qualification.
        // Ordinary typed-buffer tests separately prove a bad source cannot poison
        // a simultaneous healthy contribution. Here the native probe sees the
        // converted device samples, including samples that would be non-finite.
        let malformed_path = root.path().join("non-finite-native.wav");
        let mut malformed_writer = hound::WavWriter::create(
            &malformed_path,
            hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .expect("create IEEE-float WAV");
        for _ in 0..1_024 {
            for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.025_f32] {
                malformed_writer
                    .write_sample(sample)
                    .expect("write float sample");
            }
        }
        malformed_writer
            .finalize()
            .expect("finalize IEEE-float WAV");
        // Verify this is genuinely malformed input, rather than a sanitized fixture.
        let decoded = hound::WavReader::open(&malformed_path)
            .expect("read IEEE-float WAV")
            .samples::<f32>()
            .take(4)
            .collect::<Result<Vec<_>, _>>()
            .expect("decode float samples");
        assert!(decoded[0].is_nan());
        assert_eq!(decoded[1], f32::INFINITY);
        assert_eq!(decoded[2], f32::NEG_INFINITY);

        for volume in [0.01, 0.0] {
            let mut malformed_request = request(false, 0.0);
            malformed_request.source = PlaybackRuntimeSource::WavFile {
                path: malformed_path.clone(),
            };
            malformed_request.volume = volume;
            malformed_request.edit_fade = None;
            let before = executor.player.native_callback_sample_count(0);
            let rejected_before = executor
                .player
                .native_callback_rejected_contribution_count();
            executor
                .play(malformed_request)
                .expect("play malformed WAV natively");
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline
                && (executor.player.native_active_source_count() == 0
                    || executor.player.native_callback_sample_count(0) < before + 4_096
                    || executor
                        .player
                        .native_callback_rejected_contribution_count()
                        <= rejected_before)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(executor.player.native_active_source_count() > 0);
            assert!(executor.player.native_callback_sample_count(0) >= before + 4_096);
            assert!(
                executor
                    .player
                    .native_callback_rejected_contribution_count()
                    > rejected_before
            );
            assert_eq!(executor.player.native_callback_non_finite_sample_count(), 0);
            eprintln!(
                "native malformed WAV volume={volume}: rejected non-finite contributions; finite device output verified"
            );
        }
        executor.stop().expect("stop native playback");
        let stop_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < stop_deadline && executor.player.native_active_source_count() != 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(executor.player.native_active_source_count(), 0);
        assert_eq!(executor.player.native_callback_allocation_count(), 0);
        assert_eq!(executor.player.native_callback_deallocation_count(), 0);
        drop(executor);
        let teardown_deadline = Instant::now() + Duration::from_secs(5);
        while !teardown_complete.load(std::sync::atomic::Ordering::Acquire)
            && Instant::now() < teardown_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(teardown_complete.load(std::sync::atomic::Ordering::Acquire));
    }

    #[cfg(all(target_os = "macos", feature = "native-audio"))]
    #[test]
    #[ignore = "creates a private CoreAudio aggregate and briefly emits a low-level test signal"]
    fn macos_private_aggregate_routes_nonzero_pair_through_native_callback() {
        use crate::source::{OutputAdapter, StereoOutputPair};
        use cpal::traits::{DeviceTrait, HostTrait};
        use std::sync::atomic::Ordering as AtomicOrdering;
        use std::time::Instant;

        assert_eq!(
            std::env::var("YIELD_TEST_AUDIO_OUTPUT").ok().as_deref(),
            Some("1"),
            "set YIELD_TEST_AUDIO_OUTPUT=1 to explicitly enable real audio output"
        );

        struct StereoSignal {
            sample_rate: u32,
            right_channel: bool,
        }

        impl Iterator for StereoSignal {
            type Item = f32;

            fn next(&mut self) -> Option<Self::Item> {
                let sample = if self.right_channel { -0.3 } else { 0.2 };
                self.right_channel = !self.right_channel;
                Some(sample)
            }
        }

        impl crate::Source for StereoSignal {
            fn current_frame_len(&self) -> Option<usize> {
                None
            }

            fn channels(&self) -> u16 {
                2
            }

            fn sample_rate(&self) -> u32 {
                self.sample_rate
            }

            fn total_duration(&self) -> Option<Duration> {
                None
            }
        }

        let host_id = cpal::available_hosts()
            .into_iter()
            .find(|host| host.name().eq_ignore_ascii_case("coreaudio"))
            .expect("CoreAudio host is available");
        let host = cpal::host_from_id(host_id.clone()).expect("open CoreAudio host");
        let default_device = host
            .default_output_device()
            .expect("CoreAudio default output device");
        let default_uid = default_device.id().expect("read default output identity").1;
        let sample_rate = default_device
            .default_output_config()
            .expect("read default output format")
            .sample_rate();
        let second_uid = host
            .output_devices()
            .expect("enumerate CoreAudio outputs")
            .filter_map(|device| {
                let uid = device.id().ok()?.1;
                if uid == default_uid {
                    return None;
                }
                let channels = native_test_fixture::physical_output_channels(&uid).ok()?;
                let config = device.default_output_config().ok()?;
                (channels == 2 && config.channels() == 2 && config.sample_rate() == sample_rate)
                    .then_some(uid)
            })
            .next()
            .expect("a second stereo-capable output already running at the default rate");

        let mut guard =
            native_test_fixture::PrivateAggregateGuard::create(&default_uid, &second_uid)
                .expect("create process-private same-rate aggregate");
        let aggregate_uid = guard.uid().to_owned();
        let aggregate_name = guard.name().to_owned();
        // Private aggregates are intentionally absent from system device
        // enumeration. Bind through the exact AudioObjectID owned by the
        // guard, then verify CPAL reports that generated UID and name.
        let aggregate_device = match guard.cpal_device() {
            Ok(device) => device,
            Err(error) => {
                guard.close().expect("remove unbound private aggregate");
                panic!("bind private aggregate by its owned CoreAudio ID: {error}");
            }
        };
        assert_eq!(aggregate_device.id().unwrap().1, aggregate_uid);
        let config = crate::output::AudioOutputConfig {
            host: Some(host_id.name().to_owned()),
            device: Some(aggregate_name.clone()),
            sample_rate: Some(sample_rate),
            buffer_size: None,
        };
        let mut default_pair_started = false;
        let mut default_pair_counts = [0; 4];
        let mut default_pair_peaks = [0.0; 4];
        let mut default_pair_allocations = u64::MAX;
        let mut default_pair_deallocations = u64::MAX;
        let default_route_root = tempfile::tempdir().expect("default-pair WAV root");
        let default_route_path = default_route_root.path().join("default-pair.wav");
        let wav_spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&default_route_path, wav_spec)
            .expect("create default-pair WAV");
        for _ in 0..4_096 {
            writer
                .write_sample(8_192_i16)
                .expect("write WAV left sample");
            writer
                .write_sample(-12_288_i16)
                .expect("write WAV right sample");
        }
        writer.finalize().expect("finalize default-pair WAV");

        let default_player = match AudioPlayer::from_native_test_device(
            aggregate_device.clone(),
            host_id.name().to_owned(),
            &config,
            4,
        ) {
            Ok(player) => player,
            Err(error) => {
                // Stream startup may have constructed a native object before
                // returning its error, without exposing a teardown observer.
                guard.retain_for_process_exit();
                panic!("open exact private aggregate output: {error}");
            }
        };
        let resolved = default_player.output_details();
        let default_pair_exact = resolved.host_id == host_id.name()
            && resolved.device_name == aggregate_name
            && resolved.sample_rate == sample_rate
            && resolved.channel_count == 4
            && !resolved.used_fallback;
        let default_pair_actual = format!(
            "host={} device={:?} rate={} channels={} fallback={}",
            resolved.host_id,
            resolved.device_name,
            resolved.sample_rate,
            resolved.channel_count,
            resolved.used_fallback,
        );
        let mut executor = AudioPlayerPlaybackExecutor {
            player: default_player,
            configured_metronome_beat_count: None,
            tempo: None,
        };
        let teardown_complete = executor.player.native_stream_teardown_complete();
        guard.track_stream_teardown(Arc::clone(&teardown_complete));
        if default_pair_exact {
            let before = executor.player.native_callback_sample_count(0);
            let request = PlaybackRuntimeRequest {
                source: PlaybackRuntimeSource::WavFile {
                    path: default_route_path.clone(),
                },
                mode: PlaybackRuntimeMode::Looped {
                    start: 0.0,
                    end: 1.0,
                    offset: 0.0,
                },
                stream_policy: PlaybackRuntimeStreamPolicy::full(),
                volume: 0.025,
                anti_click_enabled: false,
                anti_click_fade_ms: 0.0,
                playback_gain: 1.0,
                playback_gain_normalization: None,
                replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
                edit_fade: None,
                metronome: None,
            };
            default_pair_started = executor.play(request).is_ok();
            let callback_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < callback_deadline
                && (executor.player.native_active_source_count() == 0
                    || executor.player.native_callback_sample_count(0) <= before
                    || executor.player.native_callback_peak(0) <= 0.006
                    || executor.player.native_callback_peak(1) <= 0.009)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            default_pair_counts = std::array::from_fn(|channel| {
                executor.player.native_callback_sample_count(channel)
            });
            default_pair_peaks =
                std::array::from_fn(|channel| executor.player.native_callback_peak(channel));
            executor
                .stop()
                .expect("stop default-pair executor playback");
            let stop_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < stop_deadline
                && executor.player.native_active_source_count() != 0
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            default_pair_allocations = executor.player.native_callback_allocation_count();
            default_pair_deallocations = executor.player.native_callback_deallocation_count();
        }
        drop(executor);
        let teardown_deadline = Instant::now() + Duration::from_secs(5);
        while !teardown_complete.load(AtomicOrdering::Acquire) && Instant::now() < teardown_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let default_pair_teardown = teardown_complete.load(AtomicOrdering::Acquire);
        if !default_pair_teardown {
            panic!("default-pair native stream teardown did not complete");
        }

        let mut pair_one_counts = [0; 4];
        let mut pair_one_peaks = [0.0; 4];
        let mut pair_one_allocations = u64::MAX;
        let mut pair_one_deallocations = u64::MAX;
        let mut player = match AudioPlayer::from_native_test_device(
            aggregate_device,
            host_id.name().to_owned(),
            &config,
            4,
        ) {
            Ok(player) => player,
            Err(error) => {
                // Stream startup may have constructed a native object before
                // returning its error, without exposing a teardown observer.
                guard.retain_for_process_exit();
                panic!("open private aggregate for pair-one callback: {error}");
            }
        };
        let resolved = player.output_details();
        let pair_one_exact = resolved.host_id == host_id.name()
            && resolved.device_name == aggregate_name
            && resolved.sample_rate == sample_rate
            && resolved.channel_count == 4
            && !resolved.used_fallback;
        let pair_one_actual = format!(
            "host={} device={:?} rate={} channels={} fallback={}",
            resolved.host_id,
            resolved.device_name,
            resolved.sample_rate,
            resolved.channel_count,
            resolved.used_fallback,
        );
        let teardown_complete = player.native_stream_teardown_complete();
        guard.track_stream_teardown(Arc::clone(&teardown_complete));
        if pair_one_exact {
            player.set_volume(1.0);
            let monitor = player.create_monitor_sink(0.025);
            let adapter = OutputAdapter::with_output_pair(
                StereoSignal {
                    sample_rate,
                    right_channel: false,
                },
                sample_rate,
                4,
                StereoOutputPair::new(1),
            );
            monitor.append(adapter);

            let callback_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < callback_deadline
                && !(player.native_callback_peak(2) > 0.004
                    && player.native_callback_peak(3) > 0.006)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            monitor.stop();
            let stop_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < stop_deadline && player.native_active_source_count() != 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            pair_one_counts =
                std::array::from_fn(|channel| player.native_callback_sample_count(channel));
            pair_one_peaks = std::array::from_fn(|channel| player.native_callback_peak(channel));
            pair_one_allocations = player.native_callback_allocation_count();
            pair_one_deallocations = player.native_callback_deallocation_count();
            drop(monitor);
        }
        drop(player);
        let teardown_deadline = Instant::now() + Duration::from_secs(5);
        while !teardown_complete.load(AtomicOrdering::Acquire) && Instant::now() < teardown_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pair_one_teardown = teardown_complete.load(AtomicOrdering::Acquire);
        if !pair_one_teardown {
            panic!("pair-one native stream teardown did not complete");
        }
        guard
            .close()
            .expect("destroy only the owned aggregate after both streams stop");

        assert!(
            default_pair_exact,
            "default route must open exact four-channel aggregate; resolved {default_pair_actual}"
        );
        assert!(
            default_pair_started,
            "production AudioPlayer executor starts its WAV"
        );
        assert!(default_pair_teardown);
        assert!(default_pair_counts.iter().all(|count| *count > 0));
        assert!(default_pair_peaks[0] > 0.006);
        assert!(default_pair_peaks[1] > 0.009);
        assert_eq!(default_pair_peaks[2], 0.0);
        assert_eq!(default_pair_peaks[3], 0.0);
        assert_eq!(default_pair_allocations, 0);
        assert_eq!(default_pair_deallocations, 0);

        assert!(
            pair_one_exact,
            "pair-one route must open exact four-channel aggregate; resolved {pair_one_actual}"
        );
        assert!(pair_one_teardown);
        assert!(pair_one_counts.iter().all(|count| *count > 0));
        assert_eq!(pair_one_peaks[0], 0.0);
        assert_eq!(pair_one_peaks[1], 0.0);
        assert!(pair_one_peaks[2] > 0.004);
        assert!(pair_one_peaks[3] > 0.006);
        assert_eq!(pair_one_allocations, 0);
        assert_eq!(pair_one_deallocations, 0);
    }

    #[derive(Clone)]
    enum FakeOutcome {
        Started(f32),
        Failed(&'static str),
    }

    struct FakeExecutor {
        outcomes: Vec<FakeOutcome>,
        played: Arc<Mutex<Vec<PlaybackRuntimeMode>>>,
        retargeted: Arc<Mutex<Vec<PlaybackRuntimeSpanUpdate>>>,
        playback_gains: Arc<Mutex<Vec<(f32, Option<PlaybackRuntimeGainNormalization>)>>>,
        anti_click_settings: Arc<Mutex<Vec<(bool, f32)>>>,
        stopped: Arc<Mutex<usize>>,
        stop_error: Option<&'static str>,
        retarget_outcomes: Vec<Result<(), &'static str>>,
        progress: Arc<Mutex<PlaybackRuntimeProgress>>,
        dropped: Arc<AtomicBool>,
        play_gate: Option<PlayGate>,
        drop_gate: Option<DropGate>,
    }

    struct PlayGate {
        entered_tx: mpsc::Sender<()>,
        release_rx: mpsc::Receiver<()>,
    }

    struct DropGate {
        entered_tx: mpsc::Sender<()>,
        release_rx: mpsc::Receiver<()>,
    }

    impl FakeExecutor {
        fn new(outcomes: Vec<FakeOutcome>) -> Self {
            Self {
                outcomes,
                played: Arc::new(Mutex::new(Vec::new())),
                retargeted: Arc::new(Mutex::new(Vec::new())),
                playback_gains: Arc::new(Mutex::new(Vec::new())),
                anti_click_settings: Arc::new(Mutex::new(Vec::new())),
                stopped: Arc::new(Mutex::new(0)),
                stop_error: None,
                retarget_outcomes: Vec::new(),
                progress: Arc::new(Mutex::new(PlaybackRuntimeProgress {
                    active: true,
                    elapsed: Some(Duration::from_millis(10)),
                    looping: false,
                    progress: Some(0.25),
                    error: None,
                    playback_span_frames: None,
                    track_total_frames: None,
                    configured_span_request_id: None,
                    metronome_beat_count: None,
                    playback_request_id: None,
                })),
                dropped: Arc::new(AtomicBool::new(false)),
                play_gate: None,
                drop_gate: None,
            }
        }

        fn played(&self) -> Arc<Mutex<Vec<PlaybackRuntimeMode>>> {
            Arc::clone(&self.played)
        }

        fn retargeted(&self) -> Arc<Mutex<Vec<PlaybackRuntimeSpanUpdate>>> {
            Arc::clone(&self.retargeted)
        }

        fn playback_gains(
            &self,
        ) -> Arc<Mutex<Vec<(f32, Option<PlaybackRuntimeGainNormalization>)>>> {
            Arc::clone(&self.playback_gains)
        }

        fn anti_click_settings(&self) -> Arc<Mutex<Vec<(bool, f32)>>> {
            Arc::clone(&self.anti_click_settings)
        }

        fn dropped(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.dropped)
        }

        fn progress_state(&self) -> Arc<Mutex<PlaybackRuntimeProgress>> {
            Arc::clone(&self.progress)
        }

        fn with_stop_error(mut self, error: &'static str) -> Self {
            self.stop_error = Some(error);
            self
        }

        fn with_retarget_outcomes(mut self, outcomes: Vec<Result<(), &'static str>>) -> Self {
            self.retarget_outcomes = outcomes;
            self
        }

        fn with_play_gate(
            mut self,
            entered_tx: mpsc::Sender<()>,
            release_rx: mpsc::Receiver<()>,
        ) -> Self {
            self.play_gate = Some(PlayGate {
                entered_tx,
                release_rx,
            });
            self
        }
    }

    impl Drop for FakeExecutor {
        fn drop(&mut self) {
            if let Some(gate) = self.drop_gate.take() {
                let _ = gate.entered_tx.send(());
                let _ = gate.release_rx.recv();
            }
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct BlockingExecutor {
        entered_tx: mpsc::Sender<()>,
        release_rx: mpsc::Receiver<()>,
        blocked_once: bool,
        played: Arc<Mutex<Vec<PlaybackRuntimeMode>>>,
    }

    impl BlockingExecutor {
        fn new(entered_tx: mpsc::Sender<()>, release_rx: mpsc::Receiver<()>) -> Self {
            Self {
                entered_tx,
                release_rx,
                blocked_once: false,
                played: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn played(&self) -> Arc<Mutex<Vec<PlaybackRuntimeMode>>> {
            Arc::clone(&self.played)
        }
    }

    impl PlaybackRuntimeExecutor for BlockingExecutor {
        fn play(
            &mut self,
            request: PlaybackRuntimeRequest,
        ) -> Result<PlaybackRuntimeStartedData, String> {
            self.played.lock().expect("played lock").push(request.mode);
            if !self.blocked_once {
                self.blocked_once = true;
                let _ = self.entered_tx.send(());
                self.release_rx.recv().expect("release blocking executor");
            }
            Ok(PlaybackRuntimeStartedData {
                output: ResolvedOutput::default(),
                playback_start: match request.mode {
                    PlaybackRuntimeMode::OneShot { start, .. } => start as f32,
                    PlaybackRuntimeMode::Looped { offset, .. } => offset as f32,
                },
            })
        }

        fn stop(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn retarget_span(&mut self, _update: PlaybackRuntimeSpanUpdate) -> Result<f32, String> {
            Ok(0.0)
        }

        fn set_volume(&mut self, _volume: f32) {}

        fn set_playback_gain(
            &mut self,
            _gain: f32,
            _normalization: Option<PlaybackRuntimeGainNormalization>,
        ) {
        }

        fn progress(&mut self) -> PlaybackRuntimeProgress {
            PlaybackRuntimeProgress::default()
        }
    }

    impl PlaybackRuntimeExecutor for FakeExecutor {
        fn play(
            &mut self,
            request: PlaybackRuntimeRequest,
        ) -> Result<PlaybackRuntimeStartedData, String> {
            self.played.lock().expect("played lock").push(request.mode);
            self.anti_click_settings
                .lock()
                .expect("anti-click settings lock")
                .push((request.anti_click_enabled, request.anti_click_fade_ms));
            if let Some(gate) = self.play_gate.take() {
                let _ = gate.entered_tx.send(());
                gate.release_rx.recv().expect("release blocking play");
            }
            match self.outcomes.pop().unwrap_or(FakeOutcome::Started(0.0)) {
                FakeOutcome::Started(playback_start) => Ok(PlaybackRuntimeStartedData {
                    output: ResolvedOutput::default(),
                    playback_start,
                }),
                FakeOutcome::Failed(error) => Err(String::from(error)),
            }
        }

        fn stop(&mut self) -> Result<(), String> {
            *self.stopped.lock().expect("stopped lock") += 1;
            self.stop_error
                .take()
                .map_or(Ok(()), |error| Err(String::from(error)))
        }

        fn retarget_span(&mut self, update: PlaybackRuntimeSpanUpdate) -> Result<f32, String> {
            self.retargeted
                .lock()
                .expect("retargeted lock")
                .push(update);
            match self.retarget_outcomes.pop().unwrap_or(Ok(())) {
                Ok(()) => Ok(update.offset as f32),
                Err(error) => Err(String::from(error)),
            }
        }

        fn set_volume(&mut self, _volume: f32) {}

        fn set_playback_gain(
            &mut self,
            gain: f32,
            normalization: Option<PlaybackRuntimeGainNormalization>,
        ) {
            self.playback_gains
                .lock()
                .expect("playback gains lock")
                .push((gain, normalization));
        }

        fn progress(&mut self) -> PlaybackRuntimeProgress {
            self.progress.lock().expect("progress lock").clone()
        }
    }

    #[test]
    fn playback_runtime_applies_scoped_gain_only_for_current_playback() {
        let executor = FakeExecutor::new(vec![]);
        let playback_gains = executor.playback_gains();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        runtime
            .handle
            .try_set_playback_gain_for(playback_id, 0.5)
            .expect("scoped gain");
        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        assert!(matches!(
            runtime.events.recv().expect("poll event"),
            PlaybackRuntimeEvent::Progress { id, .. } if id == poll_id
        ));

        assert_eq!(
            playback_gains.lock().expect("playback gains").as_slice(),
            &[(0.5, None)]
        );
    }

    #[test]
    fn playback_runtime_passes_each_playback_lifetime_anti_click_settings_to_owner() {
        let executor = FakeExecutor::new(vec![]);
        let observed = executor.anti_click_settings();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let first = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("first play");
        assert!(matches!(
            runtime.events.recv().expect("first started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == first
        ));

        let mut replacement = test_request(0.0);
        replacement.anti_click_enabled = false;
        replacement.anti_click_fade_ms = 0.0;
        let second = runtime
            .handle
            .try_play(replacement)
            .expect("replacement play");
        assert!(matches!(
            runtime.events.recv().expect("replacement started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == second
        ));
        assert_eq!(
            observed.lock().expect("anti-click settings").as_slice(),
            &[(true, 3.0), (false, 0.0)]
        );
        runtime.handle.try_shutdown().expect("shutdown");
    }

    #[test]
    fn playback_runtime_progress_reports_metronome_count_with_request_identity() {
        let executor = FakeExecutor::new(vec![]);
        {
            let progress_state = executor.progress_state();
            let mut progress = progress_state.lock().expect("progress lock");
            progress.playback_span_frames = Some((20, 80));
            progress.track_total_frames = Some(100);
            progress.metronome_beat_count = Some(4);
        }
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        let event = runtime.events.recv().expect("progress event");
        assert!(matches!(
            event,
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id
                    && progress.playback_request_id == Some(playback_id)
                    && progress.metronome_beat_count == Some(4)
        ));
    }

    #[test]
    fn playback_runtime_discards_scoped_gain_after_replacement_or_stop() {
        let executor = FakeExecutor::new(vec![]);
        let playback_gains = executor.playback_gains();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let first_playback_id = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("first play");
        assert!(matches!(
            runtime.events.recv().expect("first started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. })
                if id == first_playback_id
        ));
        let second_playback_id = runtime
            .handle
            .try_play(test_request(0.2))
            .expect("second play");
        assert!(matches!(
            runtime.events.recv().expect("second started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. })
                if id == second_playback_id
        ));

        runtime
            .handle
            .try_set_playback_gain_for(first_playback_id, 0.5)
            .expect("stale scoped gain");
        let replacement_poll_id = runtime
            .handle
            .try_poll_progress()
            .expect("replacement poll");
        assert!(matches!(
            runtime.events.recv().expect("replacement poll event"),
            PlaybackRuntimeEvent::Progress { id, .. } if id == replacement_poll_id
        ));
        assert!(playback_gains.lock().expect("playback gains").is_empty());

        let stop_id = runtime.handle.try_stop().expect("stop");
        assert!(matches!(
            runtime.events.recv().expect("stop event"),
            PlaybackRuntimeEvent::Stopped { id } if id == stop_id
        ));
        runtime
            .handle
            .try_set_playback_gain_for(second_playback_id, 0.75)
            .expect("post-stop scoped gain");
        let stopped_poll_id = runtime.handle.try_poll_progress().expect("stopped poll");
        assert!(matches!(
            runtime.events.recv().expect("stopped poll event"),
            PlaybackRuntimeEvent::Progress { id, .. } if id == stopped_poll_id
        ));
        assert!(playback_gains.lock().expect("playback gains").is_empty());
    }

    #[test]
    fn coalescing_scoped_playback_gain_preserves_identity_and_barriers() {
        let expected_playback_id = PlaybackRequestId(7);
        let newer_playback_id = PlaybackRequestId(8);
        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![scoped_playback_gain_command(expected_playback_id, 0.75)],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.75
        ));
        assert!(pending.is_empty());
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![scoped_playback_gain_command(newer_playback_id, 0.75)],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.5
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            }] if *id == newer_playback_id && *gain == 0.75
        ));
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![PlaybackRuntimeCommand::SetPlaybackGain {
                gain: 0.75,
                normalization: None,
            }],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.5
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::SetPlaybackGain { gain, normalization: None }]
                if *gain == 0.75
        ));
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![play_command(PlaybackRequestId(9), 0.4)],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.5
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::Play { id, .. }] if *id == PlaybackRequestId(9)
        ));
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![PlaybackRuntimeCommand::Stop {
                id: PlaybackRequestId(10),
            }],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.5
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::Stop { id }] if *id == PlaybackRequestId(10)
        ));
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            scoped_playback_gain_command(expected_playback_id, 0.5),
            vec![retarget_command(PlaybackRequestId(11), 0.1, 0.6)],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            } if id == expected_playback_id && gain == 0.5
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::RetargetSpan { id, .. }] if *id == PlaybackRequestId(11)
        ));
        assert!(events.is_empty());

        let (coalesced, pending, events) = coalesce_for_test(
            play_command(PlaybackRequestId(12), 0.0),
            vec![scoped_playback_gain_command(expected_playback_id, 0.5)],
        );
        assert!(matches!(
            coalesced,
            CoalescedCommand::Play { id, .. } if id == PlaybackRequestId(12)
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: id,
                gain,
            }] if *id == expected_playback_id && *gain == 0.5
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn playback_runtime_queue_preserves_scoped_gain_order_across_scopes_and_barriers() {
        let queue = PlaybackRuntimeCommandQueue::new(3);
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 0.5))
            .expect("first scoped gain");
        queue
            .try_submit(play_command(PlaybackRequestId(2), 0.0))
            .expect("play barrier");
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 0.75))
            .expect("scoped gain after play barrier");
        assert!(matches!(
            queue.try_recv().expect("first scoped gain"),
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: PlaybackRequestId(1),
                gain: 0.5,
            }
        ));
        assert!(matches!(
            queue.try_recv().expect("play barrier"),
            PlaybackRuntimeCommand::Play {
                id: PlaybackRequestId(2),
                ..
            }
        ));
        assert!(matches!(
            queue.try_recv().expect("second scoped gain"),
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: PlaybackRequestId(1),
                gain: 0.75,
            }
        ));

        let queue = PlaybackRuntimeCommandQueue::new(3);
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 0.5))
            .expect("first scoped gain");
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(2), 0.75))
            .expect("different scoped gain");
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 1.0))
            .expect("scoped gain after different scope");
        assert!(matches!(
            queue.try_recv().expect("first scoped gain"),
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: PlaybackRequestId(1),
                gain: 0.5,
            }
        ));
        assert!(matches!(
            queue.try_recv().expect("different scoped gain"),
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: PlaybackRequestId(2),
                gain: 0.75,
            }
        ));
        assert!(matches!(
            queue.try_recv().expect("second scoped gain"),
            PlaybackRuntimeCommand::SetPlaybackGainFor {
                expected_playback_id: PlaybackRequestId(1),
                gain: 1.0,
            }
        ));

        let queue = PlaybackRuntimeCommandQueue::new(2);
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 0.5))
            .expect("first scoped gain");
        queue
            .try_submit(scoped_playback_gain_command(PlaybackRequestId(2), 0.75))
            .expect("different scoped gain");
        assert_eq!(
            queue.try_submit(scoped_playback_gain_command(PlaybackRequestId(1), 1.0)),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
    }

    #[test]
    fn coalescing_keeps_stop_before_progress_poll() {
        let stop_id = PlaybackRequestId(1);
        let poll_id = PlaybackRequestId(2);
        let (coalesced, pending, events) = coalesce_for_test(
            PlaybackRuntimeCommand::Stop { id: stop_id },
            vec![PlaybackRuntimeCommand::PollProgress { id: poll_id }],
        );

        assert!(matches!(
            coalesced,
            CoalescedCommand::Stop { id } if id == stop_id
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::PollProgress { id }] if *id == poll_id
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn coalescing_keeps_stop_before_following_play() {
        let stop_id = PlaybackRequestId(1);
        let play_id = PlaybackRequestId(2);
        let (coalesced, pending, events) = coalesce_for_test(
            PlaybackRuntimeCommand::Stop { id: stop_id },
            vec![play_command(play_id, 0.25)],
        );

        assert!(matches!(
            coalesced,
            CoalescedCommand::Stop { id } if id == stop_id
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::Play { id, .. }] if *id == play_id
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn coalescing_play_then_stop_keeps_later_play_pending() {
        let first_play = PlaybackRequestId(1);
        let stop_id = PlaybackRequestId(2);
        let second_play = PlaybackRequestId(3);
        let (coalesced, pending, events) = coalesce_for_test(
            play_command(first_play, 0.0),
            vec![
                PlaybackRuntimeCommand::Stop { id: stop_id },
                play_command(second_play, 0.5),
            ],
        );

        assert!(matches!(
            coalesced,
            CoalescedCommand::Stop { id } if id == stop_id
        ));
        assert!(matches!(
            pending.as_slice(),
            [PlaybackRuntimeCommand::Play { id, .. }] if *id == second_play
        ));
        assert!(matches!(
            events.as_slice(),
            [PlaybackRuntimeEvent::Cancelled { id, reason }]
                if *id == first_play && *reason == PlaybackRuntimeCancellation::Stopped
        ));
    }

    #[test]
    fn coalescing_play_then_cancel_pending_drops_play_without_stop() {
        let play_id = PlaybackRequestId(1);
        let (coalesced, pending, events) = coalesce_for_test(
            play_command(play_id, 0.25),
            vec![PlaybackRuntimeCommand::CancelPendingPlayback],
        );

        assert!(matches!(coalesced, CoalescedCommand::CancelPendingPlayback));
        assert!(pending.is_empty());
        assert!(matches!(
            events.as_slice(),
            [PlaybackRuntimeEvent::Cancelled { id, reason }]
                if *id == play_id && *reason == PlaybackRuntimeCancellation::Stopped
        ));
    }

    #[test]
    fn playback_runtime_coalesces_queued_play_requests_to_latest() {
        let executor = FakeExecutor::new(vec![FakeOutcome::Started(0.5)]);
        let played = executor.played();
        let runtime = spawn_executor(executor, PlaybackRuntimeConfig { queue_capacity: 4 })
            .expect("spawn runtime");

        let first = runtime.handle.try_play(test_request(0.1)).expect("first");
        let second = runtime.handle.try_play(test_request(0.5)).expect("second");

        let cancelled = runtime.events.recv().expect("cancelled");
        assert!(matches!(
            cancelled,
            PlaybackRuntimeEvent::Cancelled {
                id,
                reason: PlaybackRuntimeCancellation::Superseded
            } if id == first
        ));
        let started = runtime.events.recv().expect("started");
        assert!(matches!(
            started,
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, playback_start, .. })
                if id == second && playback_start == 0.5
        ));
        assert_eq!(
            played.lock().expect("played").as_slice(),
            &[PlaybackRuntimeMode::OneShot {
                start: 0.5,
                end: 1.0
            }]
        );
    }

    #[test]
    fn playback_runtime_reports_playback_failures() {
        let runtime = spawn_executor(
            FakeExecutor::new(vec![FakeOutcome::Failed("no output")]),
            PlaybackRuntimeConfig::default(),
        )
        .expect("spawn runtime");

        let id = runtime.handle.try_play(test_request(0.0)).expect("submit");
        let event = runtime.events.recv().expect("event");

        assert!(matches!(
            event,
            PlaybackRuntimeEvent::Failed { id: event_id, error }
                if event_id == id && error == "no output"
        ));
    }

    #[test]
    fn playback_runtime_bounds_rapid_play_burst_while_executor_is_busy() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = BlockingExecutor::new(entered_tx, release_rx);
        let played = executor.played();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let first = runtime.handle.try_play(test_request(0.0)).expect("first");
        entered_rx.recv().expect("executor entered first play");
        let mut latest = first;
        let mut latest_start = 0.0;
        let mut rejected = 0;
        for index in 1..64 {
            let start = index as f64 / 100.0;
            match runtime.handle.try_play(test_request(start)) {
                Ok(id) => {
                    latest = id;
                    latest_start = start;
                }
                Err(PlaybackRuntimeSubmitError::QueueFull) => {
                    rejected += 1;
                    break;
                }
                Err(PlaybackRuntimeSubmitError::Closed) => {
                    panic!("runtime closed during rapid play burst")
                }
            }
        }
        assert!(
            rejected > 0,
            "rapid play burst should reach its bounded limit"
        );
        release_tx.send(()).expect("release executor");

        assert!(matches!(
            runtime.events.recv().expect("first started"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == first
        ));
        let mut saw_latest = false;
        while let Ok(event) = runtime.events.recv_timeout(Duration::from_secs(1)) {
            match event {
                PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. })
                    if id == latest =>
                {
                    saw_latest = true;
                    break;
                }
                PlaybackRuntimeEvent::Cancelled {
                    reason: PlaybackRuntimeCancellation::Superseded,
                    ..
                } => {}
                other => panic!("unexpected playback runtime event: {other:?}"),
            }
        }

        assert!(saw_latest, "latest rapid play request should start");
        assert_eq!(
            played.lock().expect("played").as_slice(),
            &[
                PlaybackRuntimeMode::OneShot {
                    start: 0.0,
                    end: 1.0
                },
                PlaybackRuntimeMode::OneShot {
                    start: latest_start,
                    end: 1.0
                }
            ]
        );
    }

    #[test]
    fn playback_runtime_admission_is_bounded_and_prioritizes_stop_and_shutdown() {
        let queue = PlaybackRuntimeCommandQueue::new(2);

        queue
            .try_submit(PlaybackRuntimeCommand::PollProgress {
                id: PlaybackRequestId(1),
            })
            .expect("poll admission");
        queue
            .try_submit(PlaybackRuntimeCommand::SetVolume { volume: 0.5 })
            .expect("volume admission");
        assert_eq!(queue.retained_len(), 2);
        assert_eq!(
            queue.try_submit(PlaybackRuntimeCommand::SetPlaybackGain {
                gain: 0.5,
                normalization: None,
            }),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );

        queue
            .try_submit(PlaybackRuntimeCommand::RetargetSpan {
                id: PlaybackRequestId(2),
                expected_playback_id: None,
                update: span_update(0.1, 0.4, true),
            })
            .expect("retarget evicts disposable poll");
        assert_eq!(queue.retained_len(), 2);

        let stop_id = PlaybackRequestId(3);
        queue
            .try_submit(PlaybackRuntimeCommand::Stop { id: stop_id })
            .expect("stop takes priority over disposable work");
        assert_eq!(queue.retained_len(), 2);

        queue
            .try_submit(PlaybackRuntimeCommand::Shutdown)
            .expect("shutdown remains deliverable when controls are queued");
        assert_eq!(queue.retained_len(), 2);
        assert!(matches!(
            queue.recv(),
            Some(PlaybackRuntimeCommand::Stop { id }) if id == stop_id
        ));
        assert!(matches!(
            queue.recv(),
            Some(PlaybackRuntimeCommand::Shutdown)
        ));
    }

    #[test]
    fn playback_runtime_queue_full_is_explicit_for_non_coalescible_classes() {
        let queue = PlaybackRuntimeCommandQueue::new(1);
        queue
            .try_submit(PlaybackRuntimeCommand::Stop {
                id: PlaybackRequestId(1),
            })
            .expect("critical command admission");

        assert_eq!(
            queue.try_submit(play_command(PlaybackRequestId(2), 0.2)),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(
            queue.try_submit(PlaybackRuntimeCommand::PollProgress {
                id: PlaybackRequestId(3),
            }),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(
            queue.try_submit(retarget_command(PlaybackRequestId(4), 0.2, 0.8)),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(
            queue.try_submit(PlaybackRuntimeCommand::SetVolume { volume: 0.5 }),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(
            queue.try_submit(PlaybackRuntimeCommand::SetPlaybackGain {
                gain: 0.5,
                normalization: None,
            }),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(
            queue.try_submit(scoped_playback_gain_command(PlaybackRequestId(5), 0.5)),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
    }

    #[test]
    fn playback_runtime_does_not_panic_when_critical_queue_is_full() {
        let queue = PlaybackRuntimeCommandQueue::new(1);
        queue
            .try_submit(PlaybackRuntimeCommand::Stop {
                id: PlaybackRequestId(1),
            })
            .expect("stop admission");

        assert_eq!(
            queue.try_submit(PlaybackRuntimeCommand::CancelPendingPlayback),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        queue
            .try_submit(PlaybackRuntimeCommand::Shutdown)
            .expect("shutdown should replace critical-only work");
        assert!(matches!(
            queue.recv(),
            Some(PlaybackRuntimeCommand::Shutdown)
        ));
    }

    #[test]
    fn playback_runtime_rejects_play_replacement_when_cancellation_backlog_is_full() {
        let queue = PlaybackRuntimeCommandQueue::new(2);
        queue
            .try_submit(play_command(PlaybackRequestId(1), 0.1))
            .expect("first play admission");
        queue
            .try_submit(play_command(PlaybackRequestId(2), 0.2))
            .expect("first superseding play admission");
        queue
            .try_submit(PlaybackRuntimeCommand::PollProgress {
                id: PlaybackRequestId(3),
            })
            .expect("poll admission");
        queue
            .try_submit(play_command(PlaybackRequestId(4), 0.4))
            .expect("second superseding play admission");

        assert_eq!(
            queue.try_submit(play_command(PlaybackRequestId(5), 0.5)),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        );
        assert_eq!(queue.retained_len(), 2);
        assert_eq!(
            queue.take_cancellations(),
            vec![
                (
                    PlaybackRequestId(1),
                    PlaybackRuntimeCancellation::Superseded
                ),
                (
                    PlaybackRequestId(2),
                    PlaybackRuntimeCancellation::Superseded
                ),
            ]
        );
    }

    #[test]
    fn playback_runtime_priority_commands_use_reserved_cancellation_capacity() {
        let queue = PlaybackRuntimeCommandQueue::new(1);
        queue
            .try_submit(play_command(PlaybackRequestId(1), 0.1))
            .expect("first play admission");
        queue
            .try_submit(play_command(PlaybackRequestId(2), 0.2))
            .expect("play replacement admission");

        queue
            .try_submit(PlaybackRuntimeCommand::Stop {
                id: PlaybackRequestId(3),
            })
            .expect("stop should evict a play using reserved cancellation capacity");
        assert!(matches!(
            queue.recv(),
            Some(PlaybackRuntimeCommand::Stop { id }) if id == PlaybackRequestId(3)
        ));
        assert_eq!(
            queue.take_cancellations(),
            vec![
                (
                    PlaybackRequestId(1),
                    PlaybackRuntimeCancellation::Superseded
                ),
                (PlaybackRequestId(2), PlaybackRuntimeCancellation::Stopped),
            ]
        );
    }

    #[test]
    fn playback_runtime_stays_bounded_under_blocked_poll_and_retarget_traffic() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = BlockingExecutor::new(entered_tx, release_rx);
        let runtime = spawn_executor(executor, PlaybackRuntimeConfig { queue_capacity: 4 })
            .expect("spawn runtime");
        let first = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("first play");
        entered_rx.recv().expect("executor entered first play");

        for index in 0..1_024_u64 {
            runtime
                .handle
                .try_poll_progress()
                .expect("poll should coalesce into one bounded slot");
            runtime
                .handle
                .try_retarget_span(span_update((index % 10) as f64 / 10.0, 0.9, true))
                .expect("retarget should coalesce into one bounded slot");
        }
        let stop = runtime
            .handle
            .try_stop()
            .expect("stop must remain deliverable");
        release_tx.send(()).expect("release executor");

        let mut saw_first_started = false;
        let mut saw_stop = false;
        while !saw_stop {
            match runtime
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("bounded runtime should make progress")
            {
                PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == first => {
                    saw_first_started = true
                }
                PlaybackRuntimeEvent::Stopped { id } if id == stop => saw_stop = true,
                PlaybackRuntimeEvent::Progress { .. }
                | PlaybackRuntimeEvent::Cancelled { .. }
                | PlaybackRuntimeEvent::Started(_)
                | PlaybackRuntimeEvent::Failed { .. }
                | PlaybackRuntimeEvent::Stopped { .. } => {}
            }
        }
        assert!(saw_first_started);
    }

    #[test]
    fn playback_runtime_event_queue_drops_progress_but_delivers_terminal_events() {
        let queue = Arc::new(PlaybackRuntimeEventQueue::new(1));
        let receiver = PlaybackRuntimeEventReceiver::new(Arc::clone(&queue));
        send_runtime_event(
            &queue,
            PlaybackRuntimeEvent::Progress {
                id: PlaybackRequestId(1),
                progress: PlaybackRuntimeProgress::default(),
            },
        );
        send_runtime_event(
            &queue,
            PlaybackRuntimeEvent::Progress {
                id: PlaybackRequestId(2),
                progress: PlaybackRuntimeProgress::default(),
            },
        );
        let terminal_sender = Arc::clone(&queue);
        let terminal = thread::spawn(move || {
            send_runtime_event(
                &terminal_sender,
                PlaybackRuntimeEvent::Stopped {
                    id: PlaybackRequestId(3),
                },
            );
        });
        terminal.join().expect("terminal event sender");
        assert!(matches!(
            receiver.recv().expect("terminal event"),
            PlaybackRuntimeEvent::Stopped {
                id: PlaybackRequestId(3)
            }
        ));
    }

    #[test]
    fn playback_runtime_reports_closed_submit_errors() {
        let commands = Arc::new(PlaybackRuntimeCommandQueue::new(1));
        commands.close();
        let handle = PlaybackRuntimeHandle {
            commands,
            next_id: Arc::new(AtomicU64::new(1)),
            metronome_mix: MetronomeMixOverride::default(),
        };

        assert_eq!(
            handle.try_play(test_request(0.0)),
            Err(PlaybackRuntimeSubmitError::Closed)
        );
    }

    #[test]
    fn metronome_mix_update_is_queue_independent_and_stereo_frame_coherent() {
        use crate::player::metronome::MetronomeSource;
        use crate::{MetronomeSound, SamplesBuffer};

        let handle = PlaybackRuntimeHandle {
            commands: Arc::new(PlaybackRuntimeCommandQueue::new(1)),
            next_id: Arc::new(AtomicU64::new(1)),
            metronome_mix: MetronomeMixOverride::default(),
        };
        handle.try_set_volume(0.5).expect("fill the command queue");
        let config = PlaybackMetronomeConfig::new(1).with_mix(PlaybackMetronomeMix::new(
            50,
            true,
            false,
            MetronomeSound::Classic,
        ));
        let source = || {
            MetronomeSource::new(
                SamplesBuffer::from_arc(2, 48_000, Arc::from([0.0; 4])),
                config,
                48_000,
                0,
                0,
            )
            .with_mix_override(handle.metronome_mix.clone())
        };

        let mut active = source();
        assert_eq!(active.next(), Some(0.11)); // Unset override preserves the request mix.
        handle.clone().set_metronome_mix(PlaybackMetronomeMix::new(
            0,
            true,
            false,
            MetronomeSound::Classic,
        ));
        assert_eq!(active.next(), Some(0.11)); // Second channel of the original frame.
        assert_eq!(active.next(), Some(0.0));
        assert_eq!(source().next(), Some(0.0));

        handle.set_metronome_mix(PlaybackMetronomeMix::new(
            25,
            true,
            false,
            MetronomeSound::Classic,
        ));
        assert_eq!(source().next(), Some(0.055));
        let state = handle.commands.state.lock().expect("command queue");
        assert!(matches!(
            state.commands.front(),
            Some(PlaybackRuntimeCommand::SetVolume { volume: 0.5 })
        ));
        assert_eq!(state.commands.len(), 1);
    }

    #[test]
    fn playback_runtime_closes_retained_handles_after_shutdown() {
        let runtime = spawn_executor(
            FakeExecutor::new(vec![]),
            PlaybackRuntimeConfig { queue_capacity: 1 },
        )
        .expect("spawn runtime");
        let handle = runtime.handle.clone();
        runtime.handle.try_shutdown().expect("shutdown");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match handle.try_play(test_request(0.0)) {
                Err(PlaybackRuntimeSubmitError::Closed) => return,
                Err(PlaybackRuntimeSubmitError::QueueFull) | Ok(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "retained handle should close after runtime shutdown"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[test]
    fn playback_runtime_disconnect_waits_for_executor_drop() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut executor = FakeExecutor::new(vec![]);
        let dropped = executor.dropped();
        executor.drop_gate = Some(DropGate {
            entered_tx,
            release_rx,
        });
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        runtime.handle.try_shutdown().expect("shutdown");
        let drop_entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        let disconnect_before_release = runtime.events.recv_timeout(Duration::from_millis(50));
        let release_sent = release_tx.send(()).is_ok();
        let disconnect_after_release = runtime.events.recv_timeout(Duration::from_secs(1));

        assert!(drop_entered, "executor should be dropped during shutdown");
        assert!(matches!(
            disconnect_before_release,
            Err(PlaybackRuntimeEventRecvTimeoutError::Timeout)
        ));
        assert!(
            release_sent,
            "drop gate should still own its release receiver"
        );
        assert!(matches!(
            disconnect_after_release,
            Err(PlaybackRuntimeEventRecvTimeoutError::Disconnected)
        ));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn playback_runtime_reports_progress_snapshots() {
        let runtime = spawn_executor(FakeExecutor::new(vec![]), PlaybackRuntimeConfig::default())
            .expect("spawn runtime");

        let id = runtime.handle.try_poll_progress().expect("poll");
        let event = runtime.events.recv().expect("event");

        assert!(matches!(
            event,
            PlaybackRuntimeEvent::Progress {
                id: event_id,
                progress
            } if event_id == id && progress.active && progress.progress == Some(0.25)
        ));
    }

    #[test]
    fn configured_span_ack_persists_after_a_successful_retarget() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        {
            let mut progress = progress_state.lock().expect("progress lock");
            progress.playback_span_frames = Some((12_345, 67_891));
            progress.track_total_frames = Some(96_001);
            progress.looping = true;
            progress.metronome_beat_count = Some(7);
        }

        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.1, 0.7, true))
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("retarget event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == retarget_id
                    && progress.configured_span_request_id == Some(retarget_id)
                    && progress.playback_span_frames == Some((12_345, 67_891))
                    && progress.track_total_frames == Some(96_001)
                    && progress.looping
                    && progress.metronome_beat_count == Some(7)
        ));

        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        assert!(matches!(
            runtime.events.recv().expect("poll event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id
                    && progress.configured_span_request_id == Some(retarget_id)
                    && progress.playback_span_frames == Some((12_345, 67_891))
                    && progress.track_total_frames == Some(96_001)
        ));
    }

    #[test]
    fn failed_newer_retarget_retains_the_last_successful_configured_span_ack() {
        let executor =
            FakeExecutor::new(vec![]).with_retarget_outcomes(vec![Err("B failed"), Ok(())]);
        let progress_state = executor.progress_state();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        {
            let mut progress = progress_state.lock().expect("progress lock");
            progress.playback_span_frames = Some((20, 80));
            progress.track_total_frames = Some(100);
        }

        let first = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.2, 0.8, true))
            .expect("first retarget");
        assert!(matches!(
            runtime.events.recv().expect("first event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == first && progress.configured_span_request_id == Some(first)
        ));

        let second = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.3, 0.7, false))
            .expect("second retarget");
        assert!(matches!(
            runtime.events.recv().expect("failed second event"),
            PlaybackRuntimeEvent::Failed { id, error } if id == second && error == "B failed"
        ));

        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        assert!(matches!(
            runtime.events.recv().expect("poll event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id
                    && progress.configured_span_request_id == Some(first)
                    && progress.playback_span_frames == Some((20, 80))
                    && progress.track_total_frames == Some(100)
        ));
    }

    #[test]
    fn play_and_stop_reset_the_configured_span_ack() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let first_play = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("first play");
        assert!(matches!(
            runtime.events.recv().expect("first started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == first_play
        ));
        {
            let mut progress = progress_state.lock().expect("progress lock");
            progress.playback_span_frames = Some((20, 80));
            progress.track_total_frames = Some(100);
            progress.metronome_beat_count = Some(4);
        }
        let retarget_id = runtime
            .handle
            .try_retarget_active_span(first_play, span_update(0.2, 0.8, true))
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("retarget event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == retarget_id && progress.configured_span_request_id == Some(retarget_id)
        ));

        let second_play = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("second play");
        assert!(matches!(
            runtime.events.recv().expect("second started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == second_play
        ));
        let after_play_poll = runtime.handle.try_poll_progress().expect("play reset poll");
        assert!(matches!(
            runtime.events.recv().expect("play reset event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == after_play_poll
                    && progress.configured_span_request_id.is_none()
                    && progress.playback_span_frames == Some((20, 80))
                    && progress.track_total_frames == Some(100)
        ));

        let stop_id = runtime.handle.try_stop().expect("stop");
        assert!(matches!(
            runtime.events.recv().expect("stopped event"),
            PlaybackRuntimeEvent::Stopped { id } if id == stop_id
        ));
        let after_stop_poll = runtime.handle.try_poll_progress().expect("stop reset poll");
        assert!(matches!(
            runtime.events.recv().expect("stop reset event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == after_stop_poll
                    && progress.configured_span_request_id.is_none()
                    && progress.playback_span_frames.is_none()
                    && progress.track_total_frames.is_none()
                    && progress.metronome_beat_count.is_none()
        ));
    }

    #[test]
    fn inactive_or_invalid_progress_hides_all_configured_span_metadata() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        {
            let mut progress = progress_state.lock().expect("progress lock");
            progress.active = false;
            progress.playback_span_frames = Some((80, 20));
            progress.track_total_frames = Some(100);
            progress.metronome_beat_count = Some(4);
        }

        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        assert!(matches!(
            runtime.events.recv().expect("inactive event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id
                    && !progress.active
                    && progress.playback_span_frames.is_none()
                    && progress.track_total_frames.is_none()
                    && progress.configured_span_request_id.is_none()
                    && progress.metronome_beat_count.is_none()
        ));
    }

    #[test]
    fn dispatch_passes_seek_if_outside_through_to_the_player_executor() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let mut update = span_update(0.1, 0.6, true);
        update.offset = 0.9;
        update.seek_to_offset = false;
        update.seek_if_outside = true;
        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, update)
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("retarget event"),
            PlaybackRuntimeEvent::Progress { id, .. } if id == retarget_id
        ));
        assert_eq!(retargeted.lock().expect("retargeted").as_slice(), &[update]);
    }

    #[test]
    fn dispatch_rebases_nonpolicy_retargets_but_leaves_policy_offsets_unchanged() {
        let progress = PlaybackRuntimeProgress {
            active: true,
            progress: Some(0.25),
            ..PlaybackRuntimeProgress::default()
        };
        let mut ordinary = span_update(0.1, 0.6, true);
        ordinary.offset = 0.9;
        ordinary.seek_to_offset = false;
        apply_dispatch_time_progress(&mut ordinary, &progress);
        assert_eq!(ordinary.offset, 0.25);

        let mut policy = ordinary;
        policy.offset = 0.9;
        policy.seek_if_outside = true;
        apply_dispatch_time_progress(&mut policy, &progress);
        assert_eq!(policy.offset, 0.9);
        assert!(!policy.seek_to_offset);
    }

    #[test]
    fn playback_runtime_guarded_retarget_uses_fresh_progress_for_non_seeking_updates() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let mut non_seeking = span_update(0.1, 0.6, true);
        non_seeking.offset = 0.9;
        non_seeking.seek_to_offset = false;
        let non_seeking_id = runtime
            .handle
            .try_retarget_active_span(playback_id, non_seeking)
            .expect("non-seeking retarget");
        assert!(matches!(
            runtime.events.recv().expect("non-seeking event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == non_seeking_id
                    && progress.active
                    && progress.progress == Some(0.25)
                    && progress.playback_request_id == Some(playback_id)
        ));

        let mut explicit_seek = span_update(0.1, 0.6, true);
        explicit_seek.offset = 0.4;
        let explicit_seek_id = runtime
            .handle
            .try_retarget_active_span(playback_id, explicit_seek)
            .expect("explicit-seek retarget");
        assert!(matches!(
            runtime.events.recv().expect("explicit-seek event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == explicit_seek_id
                    && progress.active
                    && progress.playback_request_id == Some(playback_id)
        ));

        assert_eq!(
            retargeted.lock().expect("retargeted").as_slice(),
            &[
                PlaybackRuntimeSpanUpdate {
                    offset: 0.25,
                    ..non_seeking
                },
                explicit_seek
            ]
        );
    }

    #[test]
    fn playback_runtime_guarded_retarget_rejects_wrong_and_completed_plays() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let wrong_id = runtime
            .handle
            .try_retarget_active_span(
                PlaybackRequestId(playback_id.get() + 100),
                span_update(0.2, 0.8, true),
            )
            .expect("wrong-play retarget");
        assert!(matches!(
            runtime.events.recv().expect("wrong-play event"),
            PlaybackRuntimeEvent::Cancelled { id, reason }
                if id == wrong_id && reason == PlaybackRuntimeCancellation::Superseded
        ));
        assert!(retargeted.lock().expect("retargeted").is_empty());

        progress_state.lock().expect("progress lock").active = false;
        let completed_id = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.2, 0.8, true))
            .expect("completed-play retarget");
        assert!(matches!(
            runtime.events.recv().expect("completed-play event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == completed_id
                    && !progress.active
                    && progress.playback_request_id == Some(playback_id)
        ));
        assert!(retargeted.lock().expect("retargeted").is_empty());
    }

    #[test]
    fn playback_runtime_guarded_retarget_preserves_preflight_errors() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let retargeted = executor.retargeted();
        progress_state.lock().expect("progress lock").error = Some(String::from("decoder error"));
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.2, 0.8, true))
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("error progress event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == retarget_id
                    && progress.active
                    && progress.playback_request_id == Some(playback_id)
                    && progress.error.as_deref() == Some("decoder error")
        ));
        assert!(retargeted.lock().expect("retargeted").is_empty());
    }

    #[test]
    fn playback_runtime_clears_guarded_retarget_identity_on_stop_and_failed_replacement() {
        let executor = FakeExecutor::new(vec![
            FakeOutcome::Failed("replacement failed"),
            FakeOutcome::Started(0.4),
            FakeOutcome::Started(0.0),
        ]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let first_play = runtime
            .handle
            .try_play(test_request(0.0))
            .expect("first play");
        assert!(matches!(
            runtime.events.recv().expect("first started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == first_play
        ));
        let stop_id = runtime.handle.try_stop().expect("stop");
        assert!(matches!(
            runtime.events.recv().expect("stop event"),
            PlaybackRuntimeEvent::Stopped { id } if id == stop_id
        ));

        let after_stop = runtime
            .handle
            .try_retarget_active_span(first_play, span_update(0.2, 0.8, true))
            .expect("after-stop retarget");
        assert!(matches!(
            runtime.events.recv().expect("after-stop event"),
            PlaybackRuntimeEvent::Cancelled { id, reason }
                if id == after_stop && reason == PlaybackRuntimeCancellation::Superseded
        ));

        let replacement_play = runtime
            .handle
            .try_play(test_request(0.4))
            .expect("replacement play");
        assert!(matches!(
            runtime.events.recv().expect("replacement started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. })
                if id == replacement_play
        ));
        let failed_play = runtime
            .handle
            .try_play(test_request(0.6))
            .expect("failed replacement");
        assert!(matches!(
            runtime.events.recv().expect("failed play event"),
            PlaybackRuntimeEvent::Failed { id, error }
                if id == failed_play && error == "replacement failed"
        ));
        let after_failure = runtime
            .handle
            .try_retarget_active_span(replacement_play, span_update(0.2, 0.8, true))
            .expect("after-failure retarget");
        assert!(matches!(
            runtime.events.recv().expect("after-failure event"),
            PlaybackRuntimeEvent::Cancelled { id, reason }
                if id == after_failure && reason == PlaybackRuntimeCancellation::Superseded
        ));
        let poll_id = runtime
            .handle
            .try_poll_progress()
            .expect("poll after failure");
        assert!(matches!(
            runtime.events.recv().expect("poll after failure event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id && progress.playback_request_id.is_none()
        ));
        assert!(retargeted.lock().expect("retargeted").is_empty());
    }

    #[test]
    fn playback_runtime_clears_guarded_retarget_identity_when_stop_fails() {
        let executor = FakeExecutor::new(vec![]).with_stop_error("stop failed");
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        let stop_id = runtime.handle.try_stop().expect("stop");
        assert!(matches!(
            runtime.events.recv().expect("failed stop event"),
            PlaybackRuntimeEvent::Failed { id, error }
                if id == stop_id && error == "stop failed"
        ));

        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, span_update(0.2, 0.8, true))
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("retarget event"),
            PlaybackRuntimeEvent::Cancelled { id, reason }
                if id == retarget_id && reason == PlaybackRuntimeCancellation::Superseded
        ));
        assert!(retargeted.lock().expect("retargeted").is_empty());
    }

    #[test]
    fn playback_runtime_cancel_pending_retains_active_play_identity() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        runtime
            .handle
            .try_cancel_pending_playback()
            .expect("cancel pending");

        let update = span_update(0.2, 0.8, true);
        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, update)
            .expect("retarget");
        assert!(matches!(
            runtime.events.recv().expect("retarget event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == retarget_id
                    && progress.active
                    && progress.playback_request_id == Some(playback_id)
        ));
        assert_eq!(retargeted.lock().expect("retargeted").as_slice(), &[update]);
    }

    #[test]
    fn playback_runtime_poll_identity_tracks_successful_play_through_completion() {
        let executor = FakeExecutor::new(vec![]);
        let progress_state = executor.progress_state();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");

        let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));
        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        assert!(matches!(
            runtime.events.recv().expect("poll event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == poll_id
                    && progress.active
                    && progress.playback_request_id == Some(playback_id)
        ));

        progress_state.lock().expect("progress lock").active = false;
        let completed_poll_id = runtime.handle.try_poll_progress().expect("completed poll");
        assert!(matches!(
            runtime.events.recv().expect("completed poll event"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == completed_poll_id
                    && !progress.active
                    && progress.playback_request_id == Some(playback_id)
        ));
    }

    #[test]
    fn stale_play_retarget_cannot_suppress_current_play_in_either_queue_order() {
        for stale_first in [true, false] {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let executor = FakeExecutor::new(vec![]).with_play_gate(entered_tx, release_rx);
            let retargeted = executor.retargeted();
            let runtime =
                spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn");
            let playback_id = runtime.handle.try_play(test_request(0.0)).expect("play");
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("entered play");
            let current_update = span_update(0.3, 0.9, false);
            for stale in [stale_first, !stale_first] {
                runtime
                    .handle
                    .try_retarget_active_span(
                        if stale {
                            PlaybackRequestId(100)
                        } else {
                            playback_id
                        },
                        if stale {
                            span_update(0.1, 0.4, true)
                        } else {
                            current_update
                        },
                    )
                    .expect("retarget");
            }
            release_tx.send(()).expect("release");
            let mut cancelled = 0;
            let mut progressed = 0;
            for _ in 0..3 {
                match runtime
                    .events
                    .recv_timeout(Duration::from_secs(2))
                    .expect("event")
                {
                    PlaybackRuntimeEvent::Started(_) => {}
                    PlaybackRuntimeEvent::Cancelled { .. } => cancelled += 1,
                    PlaybackRuntimeEvent::Progress { progress, .. } => {
                        assert_eq!(progress.playback_request_id, Some(playback_id));
                        progressed += 1;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert_eq!((cancelled, progressed), (1, 1));
            assert_eq!(
                retargeted.lock().expect("retargeted").as_slice(),
                &[current_update]
            );
        }
    }

    #[test]
    fn guarded_retarget_queue_preserves_other_identities_and_newest_same_identity() {
        let queue = PlaybackRuntimeCommandQueue::new(1);
        let play = PlaybackRequestId(1);
        queue
            .try_submit(active_retarget_command(
                PlaybackRequestId(3),
                play,
                0.2,
                0.8,
            ))
            .expect("current");
        assert!(matches!(
            queue.try_submit(active_retarget_command(
                PlaybackRequestId(4),
                PlaybackRequestId(99),
                0.1,
                0.4
            )),
            Err(PlaybackRuntimeSubmitError::QueueFull)
        ));
        queue
            .try_submit(active_retarget_command(
                PlaybackRequestId(2),
                play,
                0.1,
                0.4,
            ))
            .expect("old same play");
        assert!(
            matches!(queue.try_recv().expect("retained"), PlaybackRuntimeCommand::RetargetSpan { id, .. } if id == PlaybackRequestId(3))
        );
        let (coalesced, pending, _) = coalesce_for_test(
            active_retarget_command(PlaybackRequestId(3), play, 0.2, 0.8),
            vec![active_retarget_command(
                PlaybackRequestId(4),
                PlaybackRequestId(99),
                0.1,
                0.4,
            )],
        );
        assert!(
            matches!(coalesced, CoalescedCommand::RetargetSpan { id, .. } if id == PlaybackRequestId(3))
        );
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn rejected_guard_does_not_advance_retarget_watermark() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime = spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn");
        let play = runtime.handle.try_play(test_request(0.0)).expect("play");
        assert!(matches!(
            runtime
                .events
                .recv_timeout(Duration::from_secs(2))
                .expect("started"),
            PlaybackRuntimeEvent::Started(_)
        ));
        runtime
            .handle
            .commands
            .try_submit(active_retarget_command(
                PlaybackRequestId(4),
                PlaybackRequestId(99),
                0.1,
                0.4,
            ))
            .expect("stale identity");
        assert!(matches!(
            runtime
                .events
                .recv_timeout(Duration::from_secs(2))
                .expect("cancelled"),
            PlaybackRuntimeEvent::Cancelled { .. }
        ));
        runtime
            .handle
            .commands
            .try_submit(active_retarget_command(
                PlaybackRequestId(3),
                play,
                0.2,
                0.8,
            ))
            .expect("delayed current");
        assert!(
            matches!(runtime.events.recv_timeout(Duration::from_secs(2)).expect("progress"), PlaybackRuntimeEvent::Progress { id, .. } if id == PlaybackRequestId(3))
        );
        assert_eq!(retargeted.lock().expect("retargeted").len(), 1);
    }

    #[test]
    fn coalescing_span_retarget_keeps_latest_update() {
        let first_id = PlaybackRequestId(1);
        let second_id = PlaybackRequestId(2);
        let first_expected = PlaybackRequestId(7);
        let second_expected = first_expected;
        let (coalesced, pending, events) = coalesce_for_test(
            active_retarget_command(first_id, first_expected, 0.1, 0.5),
            vec![active_retarget_command(
                second_id,
                second_expected,
                0.2,
                0.7,
            )],
        );

        assert!(matches!(
            coalesced,
            CoalescedCommand::RetargetSpan {
                id,
                expected_playback_id,
                update,
            } if id == second_id
                && expected_playback_id == Some(second_expected)
                && update.start == 0.2
                && update.end == 0.7
        ));
        assert!(pending.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn coalescing_span_retarget_ignores_a_late_stale_command() {
        let latest_id = PlaybackRequestId(9);
        let stale_id = PlaybackRequestId(8);
        let (coalesced, pending, events) = coalesce_for_test(
            retarget_command(latest_id, 0.2, 0.7),
            vec![retarget_command(stale_id, 0.1, 0.5)],
        );

        assert!(matches!(
            coalesced,
            CoalescedCommand::RetargetSpan {
                id,
                expected_playback_id,
                update,
            } if id == latest_id
                && expected_playback_id.is_none()
                && update.start == 0.2
                && update.end == 0.7
        ));
        assert!(pending.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn playback_runtime_executes_span_retarget() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");
        let update = span_update(0.2, 0.8, true);

        let id = runtime.handle.try_retarget_span(update).expect("retarget");
        let event = runtime.events.recv().expect("event");

        assert!(matches!(
            event,
            PlaybackRuntimeEvent::Progress {
                id: event_id,
                progress
            } if event_id == id && progress.active
        ));
        assert_eq!(retargeted.lock().expect("retargeted").as_slice(), &[update]);
    }

    #[test]
    fn playback_runtime_does_not_apply_an_out_of_order_stale_retarget() {
        let executor = FakeExecutor::new(vec![]);
        let retargeted = executor.retargeted();
        let runtime =
            spawn_executor(executor, PlaybackRuntimeConfig::default()).expect("spawn runtime");
        let latest = span_update(0.3, 0.9, true);
        let stale = span_update(0.1, 0.4, true);

        runtime
            .handle
            .commands
            .try_submit(PlaybackRuntimeCommand::RetargetSpan {
                id: PlaybackRequestId(9),
                expected_playback_id: None,
                update: latest,
            })
            .expect("latest retarget");
        assert!(matches!(
            runtime.events.recv().expect("latest event"),
            PlaybackRuntimeEvent::Progress {
                id: PlaybackRequestId(9),
                ..
            }
        ));
        runtime
            .handle
            .commands
            .try_submit(PlaybackRuntimeCommand::RetargetSpan {
                id: PlaybackRequestId(8),
                expected_playback_id: None,
                update: stale,
            })
            .expect("stale retarget");

        assert!(
            runtime
                .events
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "stale retarget should be discarded without producing a false progress event"
        );
        assert_eq!(retargeted.lock().expect("retargeted").as_slice(), &[latest]);
    }

    #[test]
    fn runtime_normalization_reads_interleaved_f32_file_span() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("normalized-runtime.pcm");
        let mut file = File::create(&path).expect("create f32 cache");
        for sample in [
            0.1_f32, 0.1, 0.1, 0.1, 0.25, 0.5, 0.2, 0.2, 0.9, 0.9, 0.9, 0.9, 0.1, 0.1, 0.1, 0.1,
        ] {
            file.write_all(&sample.to_le_bytes()).expect("write sample");
        }
        let source = PlaybackRuntimeSource::InterleavedF32File {
            path,
            sample_count: 16,
            duration: 1.0,
            sample_rate: 48_000,
            channels: 1,
        };

        let gain = runtime_playback_gain_for_source(
            1.0,
            Some(PlaybackRuntimeGainNormalization::new(0.25, 0.5)),
            &source,
        );

        assert!((gain - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn runtime_normalization_uses_decoded_samples_without_file_io() {
        let source = PlaybackRuntimeSource::DecodedSamples {
            audio_bytes: Arc::from([]),
            samples: Arc::from([0.1_f32, -0.25, 0.5, -0.2]),
            duration: 1.0,
            sample_rate: 48_000,
            channels: 1,
        };

        let gain = runtime_playback_gain_for_source(
            1.0,
            Some(PlaybackRuntimeGainNormalization::new(0.0, 1.0)),
            &source,
        );

        assert!((gain - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn real_executor_publishes_configured_metronome_for_lazy_wav_retargets() {
        const SAMPLE_RATE: u32 = 1_000;
        const DURATION: f32 = 1.003;

        let player = match AudioPlayer::new() {
            Ok(player) => player,
            Err(error) => {
                eprintln!("skipping real executor provenance test: no audio output ({error})");
                return;
            }
        };
        let runtime = PlaybackRuntime::spawn(player, PlaybackRuntimeConfig::default())
            .expect("spawn runtime with real executor");
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("lazy-runtime.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create WAV");
        for _ in 0..1_003 {
            writer.write_sample(0_i16).expect("write sample");
        }
        writer.finalize().expect("finalize WAV");
        let request = PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile { path: path.clone() },
            mode: PlaybackRuntimeMode::Looped {
                start: 0.0,
                end: 1.0,
                offset: 0.0,
            },
            stream_policy: PlaybackRuntimeStreamPolicy::full(),
            volume: 0.0,
            anti_click_enabled: true,
            anti_click_fade_ms: 3.0,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: None,
            metronome: Some(PlaybackMetronomeConfig::new(7).with_source_cycle(400, 3)),
        };
        let playback_id = runtime.handle.try_play(request.clone()).expect("play");
        assert!(matches!(
            runtime.events.recv().expect("started event"),
            PlaybackRuntimeEvent::Started(PlaybackRuntimeStarted { id, .. }) if id == playback_id
        ));

        let play_poll_id = runtime.handle.try_poll_progress().expect("play poll");
        assert!(matches!(
            runtime.events.recv().expect("play progress"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == play_poll_id
                    && progress.active
                    && progress.playback_request_id == Some(playback_id)
                    && progress.metronome_beat_count == Some(7)
        ));

        let mut update = span_update(0.123, 0.876, true);
        update.seek_if_outside = true;
        update.metronome = Some(PlaybackMetronomeConfig::new(7).with_source_cycle(400, 3));
        let expected_plan = PlaybackSpanPlan::new(
            PlaybackSourceIdentity::new(PlaybackSourceKind::File, None),
            PlaybackChannelLayout::new(1, SAMPLE_RATE).expect("layout"),
            PlaybackSpanRequest::new(
                update.start as f32,
                update.end as f32,
                DURATION,
                true,
                PlaybackSeekBehavior::SpanStart,
            ),
        )
        .expect("expected plan");
        assert_eq!(expected_plan.track_frames(), 1_003);

        let retarget_id = runtime
            .handle
            .try_retarget_active_span(playback_id, update)
            .expect("guarded retarget");
        assert_retargeted_real_executor_progress(
            runtime.events.recv().expect("retarget progress"),
            retarget_id,
            &expected_plan,
        );

        let poll_id = runtime.handle.try_poll_progress().expect("poll");
        match runtime.events.recv().expect("later progress") {
            PlaybackRuntimeEvent::Progress { id, progress } => {
                assert_eq!(id, poll_id);
                assert_eq!(progress.configured_span_request_id, Some(retarget_id));
                assert_eq!(
                    progress.playback_span_frames,
                    Some((expected_plan.start_frame(), expected_plan.end_frame()))
                );
                assert_eq!(
                    progress.track_total_frames,
                    Some(expected_plan.track_frames())
                );
                assert_eq!(progress.metronome_beat_count, Some(7));
            }
            event => panic!("expected later progress, got {event:?}"),
        }

        let mut failed_request = request;
        failed_request.source = PlaybackRuntimeSource::WavFile {
            path: root.path().join("missing-runtime.wav"),
        };
        failed_request.metronome = Some(PlaybackMetronomeConfig::new(3));
        let failed_play_id = runtime
            .handle
            .try_play(failed_request)
            .expect("failed play");
        assert!(matches!(
            runtime.events.recv().expect("failed play event"),
            PlaybackRuntimeEvent::Failed { id, .. } if id == failed_play_id
        ));
        let failed_poll_id = runtime
            .handle
            .try_poll_progress()
            .expect("failed play poll");
        assert!(matches!(
            runtime.events.recv().expect("failed play progress"),
            PlaybackRuntimeEvent::Progress { id, progress }
                if id == failed_poll_id
                    && progress.playback_request_id.is_none()
                    && progress.metronome_beat_count.is_none()
        ));

        let stop_id = runtime.handle.try_stop().expect("stop");
        assert!(matches!(
            runtime.events.recv().expect("stopped event"),
            PlaybackRuntimeEvent::Stopped { id } if id == stop_id
        ));
        let inactive_poll_id = runtime.handle.try_poll_progress().expect("inactive poll");
        match runtime.events.recv().expect("inactive progress") {
            PlaybackRuntimeEvent::Progress { id, progress } => {
                assert_eq!(id, inactive_poll_id);
                assert!(!progress.active);
                assert!(progress.playback_span_frames.is_none());
                assert!(progress.track_total_frames.is_none());
                assert!(progress.configured_span_request_id.is_none());
                assert!(progress.metronome_beat_count.is_none());
            }
            event => panic!("expected inactive progress, got {event:?}"),
        }
        runtime.handle.try_shutdown().expect("shutdown");
    }

    fn assert_retargeted_real_executor_progress(
        event: PlaybackRuntimeEvent,
        retarget_id: PlaybackRequestId,
        expected_plan: &PlaybackSpanPlan,
    ) {
        match event {
            PlaybackRuntimeEvent::Progress { id, progress } => {
                assert_eq!(id, retarget_id);
                assert_eq!(progress.configured_span_request_id, Some(retarget_id));
                assert_eq!(
                    progress.playback_span_frames,
                    Some((expected_plan.start_frame(), expected_plan.end_frame()))
                );
                assert_eq!(
                    progress.track_total_frames,
                    Some(expected_plan.track_frames())
                );
                assert_eq!(progress.metronome_beat_count, Some(7));
            }
            event => panic!("expected retarget progress, got {event:?}"),
        }
    }

    fn test_request(start: f64) -> PlaybackRuntimeRequest {
        PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::AudioBytes {
                data: Arc::<[u8]>::from([]),
                duration: 1.0,
                sample_rate: 44_100,
                channels: 1,
            },
            mode: PlaybackRuntimeMode::OneShot { start, end: 1.0 },
            stream_policy: PlaybackRuntimeStreamPolicy::full(),
            volume: 1.0,
            anti_click_enabled: true,
            anti_click_fade_ms: 3.0,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::FadeOutPrevious,
            edit_fade: None,
            metronome: None,
        }
    }

    fn play_command(id: PlaybackRequestId, start: f64) -> PlaybackRuntimeCommand {
        PlaybackRuntimeCommand::Play {
            id,
            request: test_request(start),
        }
    }

    fn scoped_playback_gain_command(
        expected_playback_id: PlaybackRequestId,
        gain: f32,
    ) -> PlaybackRuntimeCommand {
        PlaybackRuntimeCommand::SetPlaybackGainFor {
            expected_playback_id,
            gain,
        }
    }

    fn retarget_command(id: PlaybackRequestId, start: f64, end: f64) -> PlaybackRuntimeCommand {
        PlaybackRuntimeCommand::RetargetSpan {
            id,
            expected_playback_id: None,
            update: span_update(start, end, true),
        }
    }

    fn active_retarget_command(
        id: PlaybackRequestId,
        expected_playback_id: PlaybackRequestId,
        start: f64,
        end: f64,
    ) -> PlaybackRuntimeCommand {
        PlaybackRuntimeCommand::RetargetSpan {
            id,
            expected_playback_id: Some(expected_playback_id),
            update: span_update(start, end, true),
        }
    }

    fn span_update(start: f64, end: f64, looped: bool) -> PlaybackRuntimeSpanUpdate {
        PlaybackRuntimeSpanUpdate {
            start,
            end,
            offset: start,
            seek_to_offset: true,
            seek_if_outside: false,
            looped,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            metronome: None,
        }
    }

    fn coalesce_for_test(
        current: PlaybackRuntimeCommand,
        queued: Vec<PlaybackRuntimeCommand>,
    ) -> (
        CoalescedCommand,
        Vec<PlaybackRuntimeCommand>,
        Vec<PlaybackRuntimeEvent>,
    ) {
        let capacity = queued.len().max(1);
        let command_receiver = PlaybackRuntimeCommandQueue::new(capacity);
        for command in queued {
            command_receiver.try_submit(command).expect("queue command");
        }
        let event_queue = Arc::new(PlaybackRuntimeEventQueue::new(capacity.max(8)));
        let event_receiver = PlaybackRuntimeEventReceiver::new(Arc::clone(&event_queue));
        let mut pending = VecDeque::new();
        let coalesced = coalesce_command(current, &command_receiver, &event_queue, &mut pending);
        let events = event_receiver.try_iter().collect();
        (coalesced, pending.into_iter().collect(), events)
    }
}
