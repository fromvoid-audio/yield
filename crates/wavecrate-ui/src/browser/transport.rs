#![deny(missing_docs)]
#![deny(warnings)]

//! Audio playback lifecycle for the browser.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Browser;
use super::metronome::{self, PlaybackMetronomeGrid};
use super::waveform::WaveformPlayRegion;
use gpui::Context;
use reson::{
    PlaybackRequestId, PlaybackRuntimeEvent, PlaybackRuntimeMode, PlaybackRuntimeProgress,
    PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest, PlaybackRuntimeSource,
    PlaybackRuntimeSpanUpdate, PlaybackRuntimeStarted, PlaybackRuntimeStreamPolicy,
};
use wavecrate_core::AudioFile;
use wavecrate_core::PlaybackLoopPolicy;

const PLAYBACK_MONITOR_INTERVAL: Duration = Duration::from_millis(80);
const PLAYBACK_CURSOR_MAX_INTERPOLATION: Duration = PLAYBACK_MONITOR_INTERVAL;
const PLAYBACK_PROGRESS_POLL_TIMEOUT: Duration = Duration::from_millis(400);

/// One authoritative normalized playback sample retained by the browser cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaybackCursorSample {
    pub(super) ratio: f32,
}

/// The complete playback mode that one live retarget must apply atomically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PlaybackDesiredState {
    /// Whether the accepted span should repeat.
    pub(super) looped: bool,
    /// Requested metronome divisions, or `None` when the metronome is disabled.
    pub(super) metronome_beat_count: Option<u16>,
}

/// The complete target for one live playback retarget.
///
/// Keeping the span and mode tuple together prevents a mode toggle from accidentally reusing a
/// span that was captured by a different request. The seek policy is part of the target so a
/// future explicit span edit can survive a later loop or metronome change without being silently
/// downgraded to the ordinary in-place behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaybackRetargetTarget {
    pub(super) span: WaveformPlayRegion,
    pub(super) desired: PlaybackDesiredState,
    pub(super) seek_if_outside: bool,
}

/// Playback identity captured when a live retarget is admitted by the browser.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlaybackRetargetAnchor {
    pub(super) playback_id: u64,
    pub(super) playback_epoch: u64,
    pub(super) waveform_generation: u64,
    pub(super) output_open_generation: u64,
    pub(super) path: std::path::PathBuf,
}

/// One complete live retarget target and the identity that owns it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlaybackRetargetRequest {
    pub(super) generation: u64,
    pub(super) target: PlaybackRetargetTarget,
    pub(super) anchor: PlaybackRetargetAnchor,
}

/// One admitted live playback-mode change awaiting an authoritative runtime snapshot.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingLoopRetarget {
    pub(super) retarget_id: u64,
    pub(super) request: PlaybackRetargetRequest,
    pub(super) metronome_grid: Option<PlaybackMetronomeGrid>,
    /// Metronome count the runtime should expose for this admitted update.
    ///
    /// This is captured from the exact runtime configuration at admission, after its beat-count
    /// normalization, independently of the optimistic logical target.
    pub(super) effective_metronome_beat_count: Option<u16>,
}

/// The one progress poll admitted by the browser monitor at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingProgressPoll {
    pub(super) request_id: u64,
    pub(super) playback_epoch: u64,
    pub(super) sent_at: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlaybackRetargetAcknowledgement {
    configured_span_request_id: Option<u64>,
    playback_span_frames: Option<(u64, u64)>,
    track_total_frames: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopRetargetProgressOutcome {
    Ignored,
    Failed,
    Inactive,
    Confirmed { desired_matches: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackProgressDisposition {
    Current,
    Stale,
    TerminalWithoutIdentity,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackIntentOrigin {
    Explicit,
    SelectionAutoplay,
    HistoryReplay,
    AudioOutputResume,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlaybackIntent {
    pub(super) start: f64,
    pub(super) region: Option<WaveformPlayRegion>,
    pub(super) looped: bool,
    pub(super) volume: f32,
    pub(super) path: std::path::PathBuf,
    pub(super) origin: PlaybackIntentOrigin,
    /// Last admitted or confirmed configuration, captured by output resume and deferred preparation.
    pub(super) resume_desired: Option<PlaybackDesiredState>,
}

impl PlaybackIntent {
    pub(super) fn set_volume(&mut self, volume: f32) {
        self.volume = super::volume::quantize_volume(volume);
    }
}

/// Bounded motion between two authoritative playback samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaybackCursorInterpolation {
    from: f32,
    to: f32,
    started_at: Instant,
    duration: Duration,
}

impl PlaybackCursorInterpolation {
    fn value_at(self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.started_at);
        let amount = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * amount
    }

    fn needs_sample_at(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < self.duration
    }
}

impl Browser {
    /// Start the bounded playback monitor when live runtime work needs polling.
    pub(super) fn ensure_playback_monitor(&mut self, cx: &mut Context<Self>) {
        if self.playback_monitor_running {
            return;
        }
        self.playback_monitor_generation = self.playback_monitor_generation.wrapping_add(1);
        let generation = self.playback_monitor_generation;
        self.playback_monitor_running = true;
        self.playback_monitor_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(PLAYBACK_MONITOR_INTERVAL)
                    .await;
                let keep_running = this
                    .update(cx, |view, cx| {
                        if view.playback_monitor_generation != generation {
                            return false;
                        }
                        let before = (
                            view.playing_path.clone(),
                            view.playback_status.clone(),
                            view.playback_cursor_sample.map(|sample| sample.ratio),
                            view.pending_loop_retarget.as_ref().map(|pending| {
                                (pending.retarget_id, pending.request.target.desired)
                            }),
                            view.accepted_playback_intent
                                .as_ref()
                                .map(|intent| intent.looped),
                            view.playback_metronome_beat_count,
                            view.playback_metronome_confirmed,
                        );
                        view.poll_playback_events();
                        let changed = before
                            != (
                                view.playing_path.clone(),
                                view.playback_status.clone(),
                                view.playback_cursor_sample.map(|sample| sample.ratio),
                                view.pending_loop_retarget.as_ref().map(|pending| {
                                    (pending.retarget_id, pending.request.target.desired)
                                }),
                                view.accepted_playback_intent
                                    .as_ref()
                                    .map(|intent| intent.looped),
                                view.playback_metronome_beat_count,
                                view.playback_metronome_confirmed,
                            );
                        if changed {
                            cx.notify();
                        }
                        let keep_running = view.playback_monitor_has_work();
                        if !keep_running {
                            view.playback_monitor_running = false;
                        }
                        keep_running
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    fn playback_monitor_has_work(&self) -> bool {
        self.playing_path.is_some()
            || self.pending_progress_poll.is_some()
            || self.pending_loop_retarget.is_some()
            || self.normalized_gain_runtime_pending.is_some()
    }

    /// Return the preview-relative cursor ratio for the current playback sample.
    pub(super) fn playback_cursor_ratio(&self, animations_enabled: bool) -> Option<f32> {
        if let Some(pending) = self.playback_cursor_pending {
            return preview_cursor_ratio_for_browser(self, pending);
        }
        self.playback_cursor_ratio_at(Instant::now(), animations_enabled)
    }

    /// Return whether a visible cursor interpolation still needs a UI frame.
    pub(super) fn playback_cursor_animation_pending(&self, now: Instant) -> bool {
        let Some(sample) = self.playback_cursor_sample else {
            return false;
        };
        let Some(interpolation) = self.playback_cursor_interpolation else {
            return false;
        };
        if !interpolation.needs_sample_at(now)
            || preview_cursor_ratio_for_browser(self, sample.ratio).is_none()
        {
            return false;
        }
        self.playback_cursor_ratio_at(now, true).is_some()
    }

    fn playback_cursor_ratio_at(&self, now: Instant, animations_enabled: bool) -> Option<f32> {
        let sample = self.playback_cursor_sample?;
        preview_cursor_ratio_for_browser(self, sample.ratio)?;
        let ratio = if animations_enabled {
            self.playback_cursor_interpolation
                .map(|interpolation| interpolation.value_at(now))
                .unwrap_or(sample.ratio)
        } else {
            sample.ratio
        };
        preview_cursor_ratio_for_browser(self, ratio)
    }

    fn set_playback_cursor_start(&mut self, started: &PlaybackRuntimeStarted) {
        if !started.playback_start.is_finite() || !(0.0..=1.0).contains(&started.playback_start) {
            self.clear_playback_cursor();
            return;
        }
        self.playback_cursor_pending = None;
        self.playback_cursor_sample = Some(PlaybackCursorSample {
            ratio: started.playback_start,
        });
        self.playback_cursor_interpolation = None;
    }

    fn record_playback_progress(&mut self, progress: &PlaybackRuntimeProgress) {
        let Some(ratio) = authoritative_cursor_progress(progress) else {
            self.clear_playback_cursor();
            return;
        };
        let sampled_at = Instant::now();
        let interpolation = self
            .playback_cursor_sample
            .and_then(|previous| cursor_interpolation(previous.ratio, ratio, sampled_at));
        self.playback_cursor_pending = None;
        self.playback_cursor_sample = Some(PlaybackCursorSample { ratio });
        self.playback_cursor_interpolation = interpolation;
    }

    pub(super) fn clear_playback_cursor(&mut self) {
        self.playback_cursor_pending = None;
        self.playback_cursor_sample = None;
        self.playback_cursor_interpolation = None;
    }

    pub(super) fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.playing_path.is_some() {
            self.stop_playback();
            cx.notify();
            return;
        }
        let Some(intent) = self.selected_playback_intent(0.0, PlaybackIntentOrigin::Explicit)
        else {
            cx.notify();
            return;
        };
        self.request_playback_intent(intent, cx);
    }

    pub(super) fn start_playback_from_position(&mut self, start: f64, cx: &mut Context<Self>) {
        let Some(intent) = self.selected_playback_intent(start, PlaybackIntentOrigin::Explicit)
        else {
            cx.notify();
            return;
        };
        self.request_playback_intent(intent, cx);
    }

    pub(super) fn request_selection_autoplay(&mut self, cx: &mut Context<Self>) {
        if !self.autoplay_selection {
            return;
        }
        let Some(intent) =
            self.selected_playback_intent(0.0, PlaybackIntentOrigin::SelectionAutoplay)
        else {
            cx.notify();
            return;
        };
        self.request_playback_intent(intent, cx);
    }

    pub(super) fn cancel_pending_selection_autoplay(&mut self) -> bool {
        let origin = self
            .playback_init_intent
            .as_ref()
            .map(|intent| intent.origin)
            .or_else(|| self.normalized_audition_pending_play_origin());
        if !should_cancel_pending_autoplay(self.autoplay_selection, origin) {
            return false;
        }
        self.cancel_normalized_audition();
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
        self.playback_init_intent = None;
        self.playback_waiting_for_preferences = false;
        self.clear_playback_cursor();
        true
    }

    pub(super) fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let mut desired = self.current_playback_desired_state();
        desired.looped = !desired.looped;
        let enabled = desired.looped;
        if !self.persist_loop_preference(enabled, cx) {
            return;
        }
        self.playback_policy.set_manual(enabled);
        self.apply_playback_state_change(desired, cx);
        cx.notify();
    }

    /// Apply a changed loop choice to pending playback and, when possible, the accepted play.
    pub(super) fn apply_loop_mode_change(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut desired = self.current_playback_desired_state();
        desired.looped = enabled;
        self.apply_playback_state_change_with_status(
            desired,
            format!("Loop {}", loop_mode_status_word(enabled)),
            cx,
        );
    }

    /// Apply a complete playback mode choice to pending playback and, when possible, the
    /// accepted play.
    pub(super) fn apply_playback_state_change(
        &mut self,
        desired: PlaybackDesiredState,
        cx: &mut Context<Self>,
    ) {
        self.apply_playback_state_change_with_status(
            desired,
            playback_state_status_word(desired),
            cx,
        );
    }

    fn apply_playback_state_change_with_status(
        &mut self,
        desired: PlaybackDesiredState,
        status_label: String,
        cx: &mut Context<Self>,
    ) {
        if self.playback_init_intent.is_some() {
            self.cancel_normalized_audition();
        }
        if let Some(intent) = self.playback_init_intent.as_mut()
            && intent.origin != PlaybackIntentOrigin::AudioOutputResume
        {
            intent.looped = desired.looped;
        }
        if let Some(intent) = self.audio_output_resume.as_mut()
            && intent.origin != PlaybackIntentOrigin::AudioOutputResume
        {
            intent.looped = desired.looped;
        }

        match self.submit_live_playback_retarget(desired, cx) {
            Some(Ok(())) => {
                self.playback_status = format!("{status_label} · applying to playback");
            }
            Some(Err(error)) => {
                self.playback_status = format!("{status_label} · live update unavailable: {error}");
            }
            None => {
                self.playback_status = if self.playing_path.is_some() {
                    format!("{status_label} — applies on next play")
                } else {
                    status_label
                };
            }
        }
    }

    fn submit_live_playback_retarget(
        &mut self,
        desired: PlaybackDesiredState,
        cx: &mut Context<Self>,
    ) -> Option<Result<(), String>> {
        let target = self.accepted_playback_retarget_target(desired)?;
        if self.normalized_audition.enabled() {
            let request = self.new_playback_retarget_request(target)?;
            if self.schedule_normalized_playback_retarget(request.clone(), cx) {
                return Some(Ok(()));
            }
            return self.submit_live_playback_retarget_request_with_gain(request, 1.0, cx);
        }
        self.submit_live_playback_retarget_with_gain(target, 1.0, cx)
    }

    pub(super) fn submit_live_playback_retarget_with_gain(
        &mut self,
        target: PlaybackRetargetTarget,
        playback_gain: f32,
        cx: &mut Context<Self>,
    ) -> Option<Result<(), String>> {
        let request = self.new_playback_retarget_request(target)?;
        self.submit_live_playback_retarget_request_with_gain(request, playback_gain, cx)
    }

    /// Submit a previously captured live retarget without allocating a new logical generation.
    pub(super) fn submit_live_playback_retarget_request_with_gain(
        &mut self,
        request: PlaybackRetargetRequest,
        playback_gain: f32,
        cx: &mut Context<Self>,
    ) -> Option<Result<(), String>> {
        let accepted_playback_id = self.latest_playback_request_id?;
        let path = self.playing_path.clone()?;
        let accepted = self.accepted_playback_intent.as_ref()?;
        if accepted.path != path
            || request.anchor.playback_id != accepted_playback_id.get()
            || !self.playback_retarget_anchor_is_current(&request.anchor)
        {
            return Some(Err(String::from(
                "playback retarget request is no longer current",
            )));
        }
        let target = request.target;
        if !target.span.is_valid() {
            return Some(Err(String::from("playback retarget span is unavailable")));
        }
        let metronome_grid = captured_metronome_grid_or_source(
            &path,
            self.playback_metronome_grid_path.as_deref(),
            self.playback_metronome_grid,
            || metronome::source_grid_for_span(self, &path, target.span),
        );
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            return Some(Err(String::from("audio runtime is unavailable")));
        };
        let update = PlaybackRuntimeSpanUpdate {
            start: target.span.start,
            end: target.span.end,
            offset: target.span.start,
            seek_to_offset: false,
            seek_if_outside: target.seek_if_outside,
            looped: target.desired.looped,
            playback_gain,
            playback_gain_normalization: None,
            metronome: metronome::config_for_count(
                target.desired.metronome_beat_count,
                metronome_grid,
            ),
        };
        let effective_metronome_beat_count = update.metronome.map(|config| config.beat_count());
        match handle.try_retarget_active_span(accepted_playback_id, update) {
            Ok(retarget_id) => {
                self.playback_metronome_grid = metronome_grid;
                self.playback_metronome_grid_path = Some(path.clone());
                admit_loop_retarget(
                    &mut self.pending_loop_retarget,
                    PendingLoopRetarget {
                        retarget_id: retarget_id.get(),
                        request,
                        metronome_grid,
                        effective_metronome_beat_count,
                    },
                );
                self.ensure_playback_monitor(cx);
                Some(Ok(()))
            }
            Err(error) => {
                let accepted_path = self
                    .accepted_playback_intent
                    .as_ref()
                    .map(|intent| intent.path.as_path());
                retain_pending_loop_retarget_after_failed_admission(
                    &mut self.pending_loop_retarget,
                    self.latest_playback_request_id.map(PlaybackRequestId::get),
                    self.playback_epoch,
                    self.audio_output_open_generation,
                    self.playing_path.as_deref(),
                    accepted_path,
                );
                Some(Err(format!("{error:?}")))
            }
        }
    }

    fn current_playback_desired_state(&self) -> PlaybackDesiredState {
        if self.playing_path.is_some() {
            if let Some(request) = self
                .normalized_audition_pending_playback_target
                .as_ref()
                .filter(|request| self.playback_retarget_anchor_is_current(&request.anchor))
            {
                return request.target.desired;
            }
            if let Some(pending) = self
                .pending_loop_retarget
                .as_ref()
                .filter(|pending| self.pending_loop_retarget_is_current(pending))
            {
                return pending.request.target.desired;
            }
            return PlaybackDesiredState {
                looped: self
                    .accepted_playback_intent
                    .as_ref()
                    .map(|intent| intent.looped)
                    .unwrap_or_else(|| self.playback_policy.enabled()),
                metronome_beat_count: fallback_metronome_target(
                    self.playback_metronome_beat_count,
                    self.playback_metronome_confirmed,
                    self.accepted_playback_intent
                        .as_ref()
                        .and_then(|intent| intent.resume_desired),
                    self.metronome_enabled,
                    self.beat_guide_count as u16,
                ),
            };
        }
        PlaybackDesiredState {
            looped: self.playback_policy.enabled(),
            metronome_beat_count: self
                .metronome_enabled
                .then_some(self.beat_guide_count as u16),
        }
    }

    fn accepted_playback_retarget_target(
        &self,
        desired: PlaybackDesiredState,
    ) -> Option<PlaybackRetargetTarget> {
        if let Some(request) = self
            .normalized_audition_pending_playback_target
            .as_ref()
            .filter(|request| self.playback_retarget_anchor_is_current(&request.anchor))
        {
            return Some(PlaybackRetargetTarget {
                span: request.target.span,
                desired,
                seek_if_outside: request.target.seek_if_outside,
            });
        }
        if let Some(pending) = self
            .pending_loop_retarget
            .as_ref()
            .filter(|pending| self.pending_loop_retarget_is_current(pending))
        {
            return Some(PlaybackRetargetTarget {
                span: pending.request.target.span,
                desired,
                seek_if_outside: pending.request.target.seek_if_outside,
            });
        }
        let accepted = self.accepted_playback_intent.as_ref()?;
        let span = playback_span_for_intent(accepted)?;
        Some(PlaybackRetargetTarget {
            span,
            desired,
            seek_if_outside: false,
        })
    }

    fn new_playback_retarget_request(
        &mut self,
        target: PlaybackRetargetTarget,
    ) -> Option<PlaybackRetargetRequest> {
        let playback_id = self.latest_playback_request_id?;
        let path = self.playing_path.clone()?;
        self.next_playback_retarget_generation =
            self.next_playback_retarget_generation.wrapping_add(1);
        Some(PlaybackRetargetRequest {
            generation: self.next_playback_retarget_generation,
            target,
            anchor: PlaybackRetargetAnchor {
                playback_id: playback_id.get(),
                playback_epoch: self.playback_epoch,
                waveform_generation: self.waveform_generation,
                output_open_generation: self.audio_output_open_generation,
                path,
            },
        })
    }

    pub(super) fn playback_retarget_anchor_is_current(
        &self,
        anchor: &PlaybackRetargetAnchor,
    ) -> bool {
        playback_retarget_anchor_matches(
            anchor,
            self.latest_playback_request_id.map(PlaybackRequestId::get),
            self.playback_epoch,
            self.waveform_generation,
            self.audio_output_open_generation,
            self.playing_path.as_deref(),
            self.accepted_playback_intent
                .as_ref()
                .map(|intent| intent.path.as_path()),
        )
    }

    /// Toggle the metronome preference and submit a complete live playback retarget when active.
    pub(super) fn toggle_metronome(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let mut desired = self.current_playback_desired_state();
        desired.metronome_beat_count = if desired.metronome_beat_count.is_some() {
            None
        } else {
            Some(self.beat_guide_count as u16)
        };
        self.metronome_enabled = desired.metronome_beat_count.is_some();
        self.apply_playback_state_change_with_status(
            desired,
            format!(
                "Metronome {}",
                if self.metronome_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            cx,
        );
        cx.notify();
    }

    /// Apply a changed beat-guide count to an active metronome without changing loop mode.
    pub(super) fn apply_metronome_count_change(&mut self, count: u8, cx: &mut Context<Self>) {
        if !self.metronome_enabled
            && self
                .current_playback_desired_state()
                .metronome_beat_count
                .is_none()
        {
            return;
        }
        let mut desired = self.current_playback_desired_state();
        desired.metronome_beat_count = Some(count as u16);
        self.apply_playback_state_change_with_status(
            desired,
            format!("Metronome beat count {count}"),
            cx,
        );
    }

    /// Return whether the metronome is enabled in the current confirmed or idle state.
    pub(super) fn metronome_toolbar_enabled(&self) -> bool {
        if self.playing_path.is_some() {
            self.playback_metronome_confirmed && self.playback_metronome_beat_count.is_some()
        } else {
            self.metronome_enabled
        }
    }

    /// Return whether the metronome target is still waiting for runtime or normalized confirmation.
    pub(super) fn metronome_toolbar_pending(&self) -> bool {
        (self.playing_path.is_some()
            && self.metronome_enabled
            && !self.playback_metronome_confirmed)
            || self
                .normalized_audition_pending_playback_target
                .as_ref()
                .is_some_and(|request| {
                    self.playback_retarget_anchor_is_current(&request.anchor)
                        && metronome_target_is_pending(
                            request.target.desired.metronome_beat_count,
                            self.playback_metronome_beat_count,
                            self.playback_metronome_confirmed,
                        )
                })
            || self.pending_loop_retarget.as_ref().is_some_and(|pending| {
                self.pending_loop_retarget_is_current(pending)
                    && metronome_target_is_pending(
                        pending.request.target.desired.metronome_beat_count,
                        self.playback_metronome_beat_count,
                        self.playback_metronome_confirmed,
                    )
            })
    }

    pub(super) fn loop_toolbar_enabled(&self) -> bool {
        if self.playing_path.is_some() {
            self.accepted_playback_intent
                .as_ref()
                .map(|intent| intent.looped)
                .unwrap_or_else(|| self.playback_policy.enabled())
        } else {
            self.playback_policy.enabled()
        }
    }

    pub(super) fn loop_toolbar_label(&self) -> String {
        let next = self.playback_policy.enabled();
        if self.playing_path.is_none() {
            return format!("Loop {}", loop_mode_toolbar_word(next));
        }
        let confirmed = self.loop_toolbar_enabled();
        let desired = self.current_playback_desired_state().looped;
        if desired != confirmed {
            return format!(
                "Loop {} → {}",
                loop_mode_toolbar_word(confirmed),
                loop_mode_toolbar_word(desired)
            );
        }
        if confirmed != next {
            return format!(
                "Loop {} · next {}",
                loop_mode_toolbar_word(confirmed),
                loop_mode_toolbar_word(next)
            );
        }
        format!("Loop {}", loop_mode_toolbar_word(confirmed))
    }

    fn selected_playback_intent(
        &mut self,
        start: f64,
        origin: PlaybackIntentOrigin,
    ) -> Option<PlaybackIntent> {
        let Some(index) = self.selected else {
            self.playback_status = "Select a sample first".to_string();
            return None;
        };
        let Some(entry) = self.entries.get(index).cloned() else {
            return None;
        };
        let path = entry.path.clone();
        self.playback_policy
            .prepare(entry.stable_id(), entry.playback_type);
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        {
            self.playback_status = "Audition currently supports WAV files".to_string();
            return None;
        }
        let Some(start) = normalized_playback_start(start) else {
            self.playback_status = "Invalid playback position".to_string();
            return None;
        };
        let region = self.waveform_play_region.filter(|region| region.is_valid());
        let start = region.map_or(start, |region| region.start);
        Some(PlaybackIntent {
            start,
            region,
            looped: self.playback_policy.enabled(),
            volume: super::volume::quantize_volume(self.volume),
            path,
            origin,
            resume_desired: None,
        })
    }

    pub(super) fn request_playback_intent(
        &mut self,
        intent: PlaybackIntent,
        cx: &mut Context<Self>,
    ) {
        self.random_audition_pending = None;
        super::zero_crossing::cancel_for_interruption(self);
        self.zero_crossing_queued = None;
        let Some(start) = normalized_playback_start(intent.start) else {
            self.clear_playback_cursor();
            self.playback_status = "Invalid playback position".to_string();
            cx.notify();
            return;
        };
        if intent.region.is_some_and(|region| !region.is_valid()) {
            self.clear_playback_cursor();
            self.playback_status = "Invalid playback region".to_string();
            cx.notify();
            return;
        }
        let intent = PlaybackIntent { start, ..intent };
        self.clear_playback_cursor();
        self.audio_output_resume = None;
        self.playback_cursor_pending = Some(start as f32);
        if !self.preferences.loaded() {
            self.playback_init_intent = Some(intent);
            self.playback_init_epoch = self.playback_epoch;
            self.playback_init_generation = self.waveform_generation;
            self.playback_waiting_for_preferences = true;
            self.playback_status = "Loading settings…".to_string();
            self.start_preferences_poll(cx);
            cx.notify();
            return;
        }
        self.playback_waiting_for_preferences = false;
        if self.audio_output_open_task.is_some() {
            self.playback_init_intent = Some(intent);
            self.playback_init_epoch = self.playback_epoch;
            self.playback_init_generation = self.waveform_generation;
            self.playback_status = "Starting audio…".to_string();
            cx.notify();
            return;
        }
        if self.playback.is_none() {
            self.start_playback_runtime(intent, cx);
        } else if self
            .audio_output_applied_config
            .as_ref()
            .is_none_or(|applied| applied != &self.audio_output_config)
        {
            self.playback_init_intent = Some(intent);
            self.playback_init_epoch = self.playback_epoch;
            self.playback_init_generation = self.waveform_generation;
            self.reconfigure_audio_output(cx);
        } else {
            self.queue_playback_intent(intent, cx);
        }
    }

    pub(super) fn start_playback_runtime(
        &mut self,
        intent: PlaybackIntent,
        cx: &mut Context<Self>,
    ) {
        self.playback_init_intent = Some(intent);
        self.playback_init_epoch = self.playback_epoch;
        self.playback_init_generation = self.waveform_generation;
        self.playback_status = "Starting audio…".to_string();
        self.begin_audio_output_open(cx);
    }

    fn queue_playback_intent(&mut self, intent: PlaybackIntent, cx: &mut Context<Self>) {
        self.queue_playback_intent_with_gain_and_span(intent, None, None, cx);
    }

    pub(super) fn queue_playback_intent_with_gain(
        &mut self,
        intent: PlaybackIntent,
        precomputed_gain: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        self.queue_playback_intent_with_gain_and_span(intent, precomputed_gain, None, cx);
    }

    pub(super) fn queue_playback_intent_with_gain_and_span(
        &mut self,
        intent: PlaybackIntent,
        precomputed_gain: Option<f32>,
        expected_span: Option<WaveformPlayRegion>,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            self.clear_playback_cursor();
            return;
        };
        if should_cancel_pending_autoplay(self.autoplay_selection, Some(intent.origin)) {
            self.clear_playback_cursor();
            self.playback_status = "Selection autoplay disabled".to_string();
            cx.notify();
            return;
        }
        let selected_entry = self
            .selected
            .and_then(|index| self.entries.get(index))
            .cloned();
        let Some(intent) = resolve_queued_playback_intent(
            &mut self.playback_policy,
            intent,
            selected_entry.as_ref(),
        ) else {
            self.clear_playback_cursor();
            self.playback_status = "Playback request is no longer current".to_string();
            cx.notify();
            return;
        };
        if let Some(expected_span) = expected_span
            && !playback_span_matches_preparation(&intent, expected_span)
        {
            if self.normalized_audition.enabled() && self.schedule_normalized_playback(intent, cx) {
                return;
            }
            self.clear_playback_cursor();
            self.playback_status = "Playback normalization is no longer current".to_string();
            cx.notify();
            return;
        }
        if precomputed_gain.is_none()
            && self.normalized_audition.enabled()
            && self.schedule_normalized_playback(intent.clone(), cx)
        {
            return;
        }
        let Some(start) = normalized_playback_start(intent.start) else {
            self.clear_playback_cursor();
            self.playback_status = "Invalid playback position".to_string();
            return;
        };
        let mode = playback_mode_for_intent(&intent);
        let metronome_grid = playback_span_for_intent(&intent).and_then(|span| {
            preferred_metronome_grid(
                intent.origin,
                &intent.path,
                self.playback_metronome_grid_path.as_deref(),
                self.playback_metronome_grid,
                || metronome::source_grid_for_span(self, &intent.path, span),
            )
        });
        let metronome_beat_count = metronome_count_for_intent(
            &intent,
            self.metronome_enabled
                .then_some(self.beat_guide_count as u16),
        );
        let request = PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile {
                path: intent.path.clone(),
            },
            mode,
            stream_policy: PlaybackRuntimeStreamPolicy::transient_navigation(),
            volume: super::volume::quantize_volume(intent.volume),
            playback_gain: precomputed_gain.unwrap_or(1.0),
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: None,
            metronome: metronome::config_for_count(metronome_beat_count, metronome_grid),
        };
        self.normalized_gain_runtime_pending = None;
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
        self.pending_loop_retarget = None;
        self.pending_progress_poll = None;
        self.playback_cursor_sample = None;
        self.playback_cursor_interpolation = None;
        match handle.try_play(request) {
            Ok(request_id) => {
                let record_last_played = should_record_last_played(intent.origin);
                let accepted_path = intent.path.clone();
                let accepted_at = current_unix_epoch_seconds();
                if should_record_playback_history(intent.origin) {
                    self.record_playback_history_for_intent(&intent);
                }
                self.mark_volume_applied_by_play();
                self.latest_playback_request_id = Some(request_id);
                let mut accepted = intent.clone();
                accepted.resume_desired = Some(PlaybackDesiredState {
                    looped: intent.looped,
                    metronome_beat_count,
                });
                self.accepted_playback_intent = Some(accepted);
                self.playing_path = Some(intent.path);
                self.playback_metronome_grid = metronome_grid;
                self.playback_metronome_grid_path = self.playing_path.clone();
                self.playback_metronome_beat_count = None;
                self.playback_metronome_confirmed = false;
                self.playback_cursor_pending = Some(start as f32);
                self.playback_status = "Starting…".to_string();
                if record_last_played {
                    self.record_playback_for_intent(accepted_path.as_path(), accepted_at, cx);
                }
                self.ensure_playback_monitor(cx);
            }
            Err(error) => {
                self.pending_loop_retarget = None;
                self.latest_playback_request_id = None;
                self.playing_path = None;
                self.accepted_playback_intent = None;
                self.playback_metronome_beat_count = None;
                self.playback_metronome_confirmed = false;
                self.clear_playback_cursor();
                self.playback_status = format!("Could not queue playback: {error:?}");
            }
        }
        cx.notify();
    }

    pub(super) fn stop_playback(&mut self) {
        self.random_audition_pending = None;
        super::zero_crossing::cancel_for_interruption(self);
        self.zero_crossing_queued = None;
        self.cancel_normalized_audition();
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
        self.playback_monitor_generation = self.playback_monitor_generation.wrapping_add(1);
        self.playback_monitor_running = false;
        self.pending_loop_retarget = None;
        self.pending_progress_poll = None;
        self.playback_init_intent = None;
        self.playback_waiting_for_preferences = false;
        self.audio_output_resume = None;
        self.accepted_playback_intent = None;
        self.playback_metronome_beat_count = None;
        self.playback_metronome_confirmed = false;
        self.clear_playback_cursor();
        if let Some(runtime) = self.playback.as_ref() {
            if let Ok(request_id) = runtime.handle.try_stop() {
                self.latest_playback_request_id = Some(request_id);
            } else {
                self.latest_playback_request_id = None;
            }
        } else {
            self.latest_playback_request_id = None;
        }
        self.playing_path = None;
        self.playback_status = "Stopped".to_string();
    }

    fn poll_playback_events(&mut self) {
        let now = Instant::now();
        if self
            .pending_progress_poll
            .is_some_and(|pending| progress_poll_has_expired(pending, now))
        {
            self.pending_progress_poll = None;
        }
        let events = self
            .playback
            .as_ref()
            .map(|runtime| runtime.events.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for event in events {
            match event {
                PlaybackRuntimeEvent::Started(started)
                    if playback_request_is_current(self.latest_playback_request_id, started.id) =>
                {
                    self.set_playback_cursor_start(&started);
                    if let Some(pending) = self.pending_loop_retarget.as_ref() {
                        self.set_pending_playback_status(pending.request.target.desired);
                    } else {
                        self.playback_status = "Playing".to_string();
                    }
                }
                PlaybackRuntimeEvent::Progress { id, progress } => {
                    self.handle_playback_progress(id, progress);
                }
                PlaybackRuntimeEvent::Failed { id, error }
                    if self.pending_loop_retarget_event_is_current(id) =>
                {
                    self.pending_loop_retarget = None;
                    self.playback_status = format!("Playback live update failed: {error}");
                }
                PlaybackRuntimeEvent::Stopped { id }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.pending_loop_retarget = None;
                    self.playback_metronome_beat_count = None;
                    self.playback_metronome_confirmed = false;
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.accepted_playback_intent = None;
                    self.playback_status = "Stopped".to_string();
                }
                PlaybackRuntimeEvent::Failed { id, error }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.pending_loop_retarget = None;
                    self.playback_metronome_beat_count = None;
                    self.playback_metronome_confirmed = false;
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.accepted_playback_intent = None;
                    self.playback_status = format!("Playback failed: {error}");
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if self.pending_loop_retarget_event_is_current(id) =>
                {
                    self.pending_loop_retarget = None;
                    self.playback_status = "Playback live update cancelled".to_string();
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.pending_loop_retarget = None;
                    self.playback_metronome_beat_count = None;
                    self.playback_metronome_confirmed = false;
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.accepted_playback_intent = None;
                    self.playback_status = "Playback cancelled".to_string();
                }
                _ => {}
            }
        }

        // Drain a completed poll before issuing its replacement. This keeps the
        // monitor's one outstanding request moving at the same cadence instead
        // of adding a full monitor interval between every pair of samples.
        if self.playing_path.is_some() && self.pending_progress_poll.is_none() {
            if let Some(runtime) = self.playback.as_ref() {
                if let Ok(request_id) = runtime.handle.try_poll_progress() {
                    self.pending_progress_poll = Some(PendingProgressPoll {
                        request_id: request_id.get(),
                        playback_epoch: self.playback_epoch,
                        sent_at: Instant::now(),
                    });
                }
            }
        }
        self.retry_pending_normalized_gain();
    }

    fn handle_playback_progress(
        &mut self,
        id: PlaybackRequestId,
        progress: PlaybackRuntimeProgress,
    ) {
        if let Some(pending) = self.pending_loop_retarget.clone()
            && pending.retarget_id == id.get()
        {
            if !self.pending_loop_retarget_is_current(&pending) {
                self.pending_loop_retarget = None;
                return;
            }
            self.reconcile_loop_retarget_progress(id.get(), progress);
            return;
        }
        if !progress_poll_is_current(self.pending_progress_poll, id.get(), self.playback_epoch) {
            return;
        }
        self.pending_progress_poll = None;
        match playback_progress_disposition(
            self.latest_playback_request_id.map(PlaybackRequestId::get),
            progress.playback_request_id.map(PlaybackRequestId::get),
            progress.error.is_some(),
        ) {
            PlaybackProgressDisposition::Error => {
                let status = progress.error.as_deref().map_or_else(
                    || "Playback failed".to_string(),
                    |error| format!("Playback failed: {error}"),
                );
                self.clear_current_playback(&status);
                return;
            }
            PlaybackProgressDisposition::TerminalWithoutIdentity => {
                self.clear_current_playback("Playback ended before it was confirmed");
                return;
            }
            PlaybackProgressDisposition::Stale => return,
            PlaybackProgressDisposition::Current => {}
        }
        if let Some(pending) = self.pending_loop_retarget.clone() {
            if !self.pending_loop_retarget_is_current(&pending) {
                self.pending_loop_retarget = None;
            } else if id.get() > pending.retarget_id {
                self.reconcile_loop_retarget_progress(id.get(), progress);
                return;
            }
        }
        self.apply_ordinary_progress(progress);
    }

    fn reconcile_loop_retarget_progress(
        &mut self,
        response_id: u64,
        progress: PlaybackRuntimeProgress,
    ) {
        let accepted_path = self
            .accepted_playback_intent
            .as_ref()
            .map(|intent| intent.path.clone());
        let outcome = reduce_playback_retarget_progress(
            &mut self.pending_loop_retarget,
            &mut self.accepted_playback_intent,
            response_id,
            self.latest_playback_request_id.map(PlaybackRequestId::get),
            self.playback_epoch,
            self.audio_output_open_generation,
            self.playing_path.as_deref(),
            accepted_path.as_deref(),
            progress.active,
            progress.error.is_some(),
            progress.playback_request_id.map(PlaybackRequestId::get),
            progress.looping,
            progress.metronome_beat_count,
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: progress
                    .configured_span_request_id
                    .map(PlaybackRequestId::get),
                playback_span_frames: progress.playback_span_frames,
                track_total_frames: progress.track_total_frames,
            },
        );
        match outcome {
            LoopRetargetProgressOutcome::Ignored => {}
            LoopRetargetProgressOutcome::Failed => {
                if let Some(error) = progress.error.as_deref() {
                    self.playback_status = format!("Playback live update failed: {error}");
                } else {
                    self.playback_status = "Playback live update failed".to_string();
                }
            }
            LoopRetargetProgressOutcome::Inactive => self.clear_current_playback("Stopped"),
            LoopRetargetProgressOutcome::Confirmed { desired_matches } => {
                self.record_confirmed_metronome(&progress);
                self.playback_metronome_confirmed = true;
                self.record_playback_progress(&progress);
                if desired_matches {
                    self.set_playback_status_from_progress(&progress);
                } else {
                    self.playback_status =
                        "Playback live update failed: playback mode did not change".to_string();
                }
            }
        }
    }

    fn record_confirmed_metronome(&mut self, progress: &PlaybackRuntimeProgress) {
        self.playback_metronome_beat_count = progress.metronome_beat_count;
        if let Some(accepted) = self.accepted_playback_intent.as_mut() {
            accepted.resume_desired = Some(PlaybackDesiredState {
                looped: progress.looping,
                metronome_beat_count: progress.metronome_beat_count,
            });
        }
    }

    fn apply_ordinary_progress(&mut self, progress: PlaybackRuntimeProgress) {
        if let Some(error) = progress.error {
            self.clear_current_playback(&format!("Playback failed: {error}"));
        } else if progress.active {
            self.record_playback_progress(&progress);
            self.record_confirmed_metronome(&progress);
            self.playback_metronome_confirmed = true;
            if self.pending_loop_retarget.is_some() {
                if let Some(pending) = self.pending_loop_retarget.as_ref() {
                    self.set_pending_playback_status(pending.request.target.desired);
                }
            } else {
                reconcile_playback_intent_mode(
                    self.accepted_playback_intent.as_mut(),
                    progress.looping,
                );
                self.set_playback_status_from_progress(&progress);
            }
        } else {
            self.clear_current_playback("Stopped");
        }
    }

    fn set_playback_status_from_progress(&mut self, progress: &PlaybackRuntimeProgress) {
        if let Some(progress) = progress.progress {
            let percent = progress * 100.0;
            self.playback_status = format!("Playing {percent:.0}%");
        } else {
            self.playback_status = "Playing".to_string();
        }
    }

    fn set_pending_playback_status(&mut self, desired: PlaybackDesiredState) {
        self.playback_status = format!(
            "{} · applying to playback",
            playback_state_status_word(desired)
        );
    }

    fn clear_current_playback(&mut self, status: &str) {
        self.cancel_normalized_audition();
        self.pending_progress_poll = None;
        self.pending_loop_retarget = None;
        self.clear_playback_cursor();
        self.latest_playback_request_id = None;
        self.playing_path = None;
        self.accepted_playback_intent = None;
        self.playback_metronome_beat_count = None;
        self.playback_metronome_confirmed = false;
        self.playback_status = status.to_string();
    }

    fn pending_loop_retarget_is_current(&self, pending: &PendingLoopRetarget) -> bool {
        self.playback_retarget_anchor_is_current(&pending.request.anchor)
    }

    fn pending_loop_retarget_event_is_current(&self, id: PlaybackRequestId) -> bool {
        self.pending_loop_retarget.as_ref().is_some_and(|pending| {
            pending.retarget_id == id.get() && self.pending_loop_retarget_is_current(pending)
        })
    }

    fn capture_audio_output_resume_intent(&self) -> Option<PlaybackIntent> {
        if let Some(intent) = self.playback_init_intent.clone() {
            return Some(intent);
        }
        let selected_path = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.as_path());
        if let Some(resume) = self.audio_output_resume.as_ref()
            && audio_output_resume_is_current(
                self.audio_output_resume_epoch,
                self.audio_output_resume_generation,
                self.playback_epoch,
                self.waveform_generation,
                selected_path,
                resume.path.as_path(),
            )
        {
            return Some(pending_resume_intent(resume, self.volume));
        }
        if let Some(resume) = self.current_normalized_resume_intent() {
            return Some(resume);
        }
        let playing_path = self.playing_path.as_ref()?;
        if selected_path != Some(playing_path) {
            return None;
        }
        let accepted = self.accepted_playback_intent.as_ref()?;
        if accepted.path != *playing_path {
            return None;
        }
        let pending = self
            .pending_loop_retarget
            .as_ref()
            .filter(|pending| self.pending_loop_retarget_is_current(pending));
        let normalized = self.current_normalized_playback_target();
        Some(capture_resume_configuration(
            accepted,
            self.volume,
            self.playback_metronome_beat_count,
            pending,
            normalized.as_ref(),
        ))
    }

    pub(super) fn reconfigure_audio_output(&mut self, cx: &mut Context<Self>) {
        let resume = self.capture_audio_output_resume_intent();
        self.stop_playback();
        if let Some(runtime) = self.playback.take() {
            self.retire_audio_output_runtime(runtime);
        }
        self.audio_output_resume = resume;
        self.audio_output_resume_epoch = self.playback_epoch;
        self.audio_output_resume_generation = self.waveform_generation;
        self.audio_output_resolved = None;
        self.audio_output_applied_config = None;
        self.audio_output_status = "Audio output change pending".to_string();
        self.begin_audio_output_open(cx);
    }

    pub(super) fn resume_audio_output_playback_if_current(&mut self, cx: &mut Context<Self>) {
        let resume = self.audio_output_resume.take();
        let pending = resume.clone().or_else(|| self.playback_init_intent.take());
        let Some(intent) = pending else {
            self.playback_status = "Ready".to_string();
            return;
        };
        let should_queue = if resume.is_some() {
            let selected_path = self
                .selected
                .and_then(|index| self.entries.get(index))
                .map(|entry| entry.path.as_path());
            audio_output_resume_is_current(
                self.audio_output_resume_epoch,
                self.audio_output_resume_generation,
                self.playback_epoch,
                self.waveform_generation,
                selected_path,
                intent.path.as_path(),
            )
        } else {
            let selected_path = self
                .selected
                .and_then(|index| self.entries.get(index))
                .map(|entry| entry.path.as_path());
            should_queue_initialized_playback(
                self.playback_init_generation,
                self.playback_init_epoch,
                self.waveform_generation,
                self.playback_epoch,
                selected_path,
                intent.path.as_path(),
            )
        };
        if should_queue {
            self.queue_playback_intent(intent, cx);
        } else {
            self.clear_playback_cursor();
            self.playback_status = "Ready".to_string();
        }
    }
}

fn resolve_queued_playback_intent(
    policy: &mut PlaybackLoopPolicy,
    mut intent: PlaybackIntent,
    selected_entry: Option<&AudioFile>,
) -> Option<PlaybackIntent> {
    let entry = selected_entry?;
    (entry.path == intent.path).then(|| {
        if intent.origin != PlaybackIntentOrigin::AudioOutputResume {
            policy.prepare(entry.stable_id(), entry.playback_type);
            intent.looped = policy.enabled();
        }
        intent
    })
}

fn capture_resume_configuration(
    accepted: &PlaybackIntent,
    volume: f32,
    confirmed_metronome: Option<u16>,
    pending: Option<&PendingLoopRetarget>,
    normalized: Option<&PlaybackRetargetRequest>,
) -> PlaybackIntent {
    let mut resume = accepted_resume_intent(accepted, volume);
    resume.resume_desired = Some(PlaybackDesiredState {
        looped: accepted.looped,
        metronome_beat_count: accepted
            .resume_desired
            .map_or(confirmed_metronome, |desired| desired.metronome_beat_count),
    });
    apply_pending_loop_retarget_to_resume(&mut resume, pending, true);
    if let Some(request) = normalized {
        resume.looped = request.target.desired.looped;
        resume.resume_desired = Some(request.target.desired);
        resume.region = Some(request.target.span);
    }
    resume
}

fn metronome_count_for_intent(intent: &PlaybackIntent, preference: Option<u16>) -> Option<u16> {
    if intent.origin == PlaybackIntentOrigin::AudioOutputResume {
        intent
            .resume_desired
            .map_or(preference, |desired| desired.metronome_beat_count)
    } else {
        preference
    }
}

fn accepted_resume_intent(accepted: &PlaybackIntent, volume: f32) -> PlaybackIntent {
    let mut resume = accepted.clone();
    resume.origin = PlaybackIntentOrigin::AudioOutputResume;
    resume.set_volume(volume);
    resume
}

fn pending_resume_intent(resume: &PlaybackIntent, volume: f32) -> PlaybackIntent {
    let mut resume = resume.clone();
    resume.set_volume(volume);
    resume
}

fn should_queue_initialized_playback(
    requested_generation: u64,
    requested_epoch: u64,
    current_generation: u64,
    current_epoch: u64,
    selected_path: Option<&Path>,
    requested_path: &Path,
) -> bool {
    requested_generation == current_generation
        && requested_epoch == current_epoch
        && selected_path == Some(requested_path)
}

fn audio_output_resume_is_current(
    resume_epoch: u64,
    resume_generation: u64,
    current_epoch: u64,
    current_generation: u64,
    selected_path: Option<&Path>,
    resume_path: &Path,
) -> bool {
    resume_epoch == current_epoch
        && resume_generation == current_generation
        && selected_path == Some(resume_path)
}

fn playback_mode_for_intent(intent: &PlaybackIntent) -> PlaybackRuntimeMode {
    let start = normalized_playback_start(intent.start).unwrap_or(0.0);
    if let Some(region) = intent.region.filter(|region| region.is_valid()) {
        if intent.looped {
            return PlaybackRuntimeMode::Looped {
                start: region.start,
                end: region.end,
                offset: region.start,
            };
        }
        return PlaybackRuntimeMode::OneShot {
            start: region.start,
            end: region.end,
        };
    }
    if intent.looped {
        PlaybackRuntimeMode::Looped {
            start: 0.0,
            end: 1.0,
            offset: start,
        }
    } else {
        PlaybackRuntimeMode::OneShot { start, end: 1.0 }
    }
}

/// Resolve the exact normalized span represented by a playback intent.
pub(super) fn playback_span_for_intent(intent: &PlaybackIntent) -> Option<WaveformPlayRegion> {
    if let Some(region) = intent.region.filter(|region| region.is_valid()) {
        return Some(region);
    }
    let start = normalized_playback_start(intent.start)?;
    if intent.looped {
        WaveformPlayRegion::new(0.0, 1.0)
    } else {
        WaveformPlayRegion::new(start, 1.0)
    }
}

/// Return whether a prepared normalized span still matches a resolved playback intent.
pub(super) fn playback_span_matches_preparation(
    intent: &PlaybackIntent,
    prepared_span: WaveformPlayRegion,
) -> bool {
    playback_span_for_intent(intent) == Some(prepared_span)
}

fn preferred_metronome_grid(
    origin: PlaybackIntentOrigin,
    requested_path: &Path,
    captured_path: Option<&Path>,
    captured_grid: Option<PlaybackMetronomeGrid>,
    source_grid: impl FnOnce() -> Option<PlaybackMetronomeGrid>,
) -> Option<PlaybackMetronomeGrid> {
    if origin == PlaybackIntentOrigin::AudioOutputResume && captured_path == Some(requested_path) {
        captured_grid
    } else {
        source_grid()
    }
}

fn captured_metronome_grid_or_source(
    requested_path: &Path,
    captured_path: Option<&Path>,
    captured_grid: Option<PlaybackMetronomeGrid>,
    source_grid: impl FnOnce() -> Option<PlaybackMetronomeGrid>,
) -> Option<PlaybackMetronomeGrid> {
    if captured_path == Some(requested_path) {
        captured_grid
    } else {
        source_grid()
    }
}

fn reconcile_playback_intent_mode(intent: Option<&mut PlaybackIntent>, looped: bool) {
    let Some(intent) = intent else {
        return;
    };
    if intent.looped == looped {
        return;
    }
    let span = playback_span_for_intent(intent);
    intent.looped = looped;
    if let Some(span) = span {
        intent.region = Some(span);
    }
}

fn admit_loop_retarget(pending: &mut Option<PendingLoopRetarget>, candidate: PendingLoopRetarget) {
    *pending = Some(candidate);
}

fn loop_retarget_fences_match(
    pending: &PendingLoopRetarget,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    output_open_generation: u64,
    playing_path: Option<&Path>,
    accepted_path: Option<&Path>,
) -> bool {
    let anchor = &pending.request.anchor;
    latest_playback_id == Some(anchor.playback_id)
        && anchor.playback_epoch == playback_epoch
        && anchor.output_open_generation == output_open_generation
        && playing_path == Some(anchor.path.as_path())
        && accepted_path == Some(anchor.path.as_path())
}

fn playback_retarget_anchor_matches(
    anchor: &PlaybackRetargetAnchor,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    waveform_generation: u64,
    output_open_generation: u64,
    playing_path: Option<&Path>,
    accepted_path: Option<&Path>,
) -> bool {
    latest_playback_id == Some(anchor.playback_id)
        && playback_epoch == anchor.playback_epoch
        && waveform_generation == anchor.waveform_generation
        && output_open_generation == anchor.output_open_generation
        && playing_path == Some(anchor.path.as_path())
        && accepted_path == Some(anchor.path.as_path())
}

fn retain_pending_loop_retarget_after_failed_admission(
    pending: &mut Option<PendingLoopRetarget>,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    output_open_generation: u64,
    playing_path: Option<&Path>,
    accepted_path: Option<&Path>,
) -> bool {
    let retain = pending.as_ref().is_some_and(|pending| {
        loop_retarget_fences_match(
            pending,
            latest_playback_id,
            playback_epoch,
            output_open_generation,
            playing_path,
            accepted_path,
        )
    });
    if !retain {
        *pending = None;
    }
    retain
}

fn apply_pending_loop_retarget_to_resume(
    resume: &mut PlaybackIntent,
    pending: Option<&PendingLoopRetarget>,
    pending_is_current: bool,
) {
    if let Some(pending) = pending.filter(|_| pending_is_current) {
        resume.looped = pending.request.target.desired.looped;
        resume.resume_desired = Some(pending.request.target.desired);
        resume.region = Some(pending.request.target.span);
    }
}

#[cfg(test)]
fn acknowledgement_for_pending(pending: &PendingLoopRetarget) -> PlaybackRetargetAcknowledgement {
    let span = pending.request.target.span;
    let total_frames = 100_u64;
    PlaybackRetargetAcknowledgement {
        configured_span_request_id: Some(pending.retarget_id),
        playback_span_frames: Some((
            (span.start * total_frames as f64).round() as u64,
            (span.end * total_frames as f64).round() as u64,
        )),
        track_total_frames: Some(total_frames),
    }
}

#[cfg(test)]
fn reduce_loop_retarget_progress(
    pending: &mut Option<PendingLoopRetarget>,
    accepted: &mut Option<PlaybackIntent>,
    response_id: u64,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    output_open_generation: u64,
    playing_path: Option<&Path>,
    accepted_path: Option<&Path>,
    active: bool,
    has_error: bool,
    playback_id: Option<u64>,
    looping: bool,
) -> LoopRetargetProgressOutcome {
    let acknowledgement = pending
        .as_ref()
        .map(acknowledgement_for_pending)
        .unwrap_or_default();
    reduce_playback_retarget_progress(
        pending,
        accepted,
        response_id,
        latest_playback_id,
        playback_epoch,
        output_open_generation,
        playing_path,
        accepted_path,
        active,
        has_error,
        playback_id,
        looping,
        None,
        acknowledgement,
    )
}

fn reduce_playback_retarget_progress(
    pending: &mut Option<PendingLoopRetarget>,
    accepted: &mut Option<PlaybackIntent>,
    response_id: u64,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    output_open_generation: u64,
    playing_path: Option<&Path>,
    accepted_path: Option<&Path>,
    active: bool,
    has_error: bool,
    playback_id: Option<u64>,
    looping: bool,
    metronome_beat_count: Option<u16>,
    acknowledgement: PlaybackRetargetAcknowledgement,
) -> LoopRetargetProgressOutcome {
    let Some(current) = pending.as_ref() else {
        return LoopRetargetProgressOutcome::Ignored;
    };
    if response_id < current.retarget_id
        || !loop_retarget_fences_match(
            current,
            latest_playback_id,
            playback_epoch,
            output_open_generation,
            playing_path,
            accepted_path,
        )
    {
        return LoopRetargetProgressOutcome::Ignored;
    }
    if playback_id.is_some() && playback_id != Some(current.request.anchor.playback_id) {
        return LoopRetargetProgressOutcome::Ignored;
    }
    if has_error {
        *pending = None;
        return LoopRetargetProgressOutcome::Failed;
    }
    if !active || playback_id.is_none() {
        *pending = None;
        return LoopRetargetProgressOutcome::Inactive;
    }
    let Some(applied_span) = runtime_playback_region(acknowledgement, current.retarget_id) else {
        return LoopRetargetProgressOutcome::Ignored;
    };
    if metronome_beat_count != current.effective_metronome_beat_count {
        return LoopRetargetProgressOutcome::Ignored;
    }
    let target = current.request.target;
    if looping != target.desired.looped {
        return LoopRetargetProgressOutcome::Ignored;
    }
    let path = current.request.anchor.path.clone();
    *pending = None;
    if let Some(intent) = accepted.as_mut()
        && intent.path == path
    {
        intent.looped = looping;
        intent.region = Some(applied_span);
    }
    LoopRetargetProgressOutcome::Confirmed {
        desired_matches: looping == target.desired.looped
            && metronome_beat_count == target.desired.metronome_beat_count,
    }
}

fn runtime_playback_region(
    acknowledgement: PlaybackRetargetAcknowledgement,
    expected_request_id: u64,
) -> Option<WaveformPlayRegion> {
    if acknowledgement.configured_span_request_id != Some(expected_request_id) {
        return None;
    }
    let ((start_frame, end_frame), total_frames) = (
        acknowledgement.playback_span_frames?,
        acknowledgement.track_total_frames?,
    );
    if start_frame >= end_frame || end_frame > total_frames || total_frames == 0 {
        return None;
    }
    let start = start_frame as f64 / total_frames as f64;
    let end = end_frame as f64 / total_frames as f64;
    WaveformPlayRegion::new(start, end)
}

fn playback_progress_disposition(
    latest_playback_id: Option<u64>,
    playback_id: Option<u64>,
    has_error: bool,
) -> PlaybackProgressDisposition {
    if playback_id.is_some() && playback_id != latest_playback_id {
        PlaybackProgressDisposition::Stale
    } else if has_error {
        PlaybackProgressDisposition::Error
    } else if playback_id.is_none() {
        PlaybackProgressDisposition::TerminalWithoutIdentity
    } else {
        PlaybackProgressDisposition::Current
    }
}

fn loop_mode_status_word(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

fn loop_mode_toolbar_word(enabled: bool) -> &'static str {
    if enabled { "On" } else { "Off" }
}

fn playback_state_status_word(desired: PlaybackDesiredState) -> String {
    if desired.metronome_beat_count.is_some() {
        format!(
            "Loop {} · metronome enabled",
            loop_mode_status_word(desired.looped)
        )
    } else {
        format!(
            "Loop {} · metronome disabled",
            loop_mode_status_word(desired.looped)
        )
    }
}

fn metronome_target_is_pending(
    desired: Option<u16>,
    confirmed: Option<u16>,
    has_confirmed_progress: bool,
) -> bool {
    desired != confirmed || (desired.is_some() && !has_confirmed_progress)
}

fn fallback_metronome_target(
    confirmed: Option<u16>,
    has_confirmed_progress: bool,
    accepted_desired: Option<PlaybackDesiredState>,
    preference_enabled: bool,
    preference_count: u16,
) -> Option<u16> {
    if has_confirmed_progress {
        return confirmed;
    }
    if let Some(desired) = accepted_desired {
        return desired.metronome_beat_count;
    }
    preference_enabled.then_some(preference_count)
}

fn normalized_playback_start(start: f64) -> Option<f64> {
    start.is_finite().then(|| start.clamp(0.0, 1.0))
}

fn should_cancel_pending_autoplay(
    autoplay_enabled: bool,
    origin: Option<PlaybackIntentOrigin>,
) -> bool {
    !autoplay_enabled && origin == Some(PlaybackIntentOrigin::SelectionAutoplay)
}

fn should_record_playback_history(origin: PlaybackIntentOrigin) -> bool {
    matches!(
        origin,
        PlaybackIntentOrigin::Explicit | PlaybackIntentOrigin::SelectionAutoplay
    )
}

fn should_record_last_played(origin: PlaybackIntentOrigin) -> bool {
    matches!(
        origin,
        PlaybackIntentOrigin::Explicit
            | PlaybackIntentOrigin::SelectionAutoplay
            | PlaybackIntentOrigin::HistoryReplay
    )
}

fn current_unix_epoch_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn preview_cursor_ratio_for_browser(browser: &Browser, ratio: f32) -> Option<f32> {
    browser.waveform.as_ref()?;
    preview_cursor_ratio(ratio)
}

/// Validate and clamp one normalized playback position for the bounded preview.
fn preview_cursor_ratio(ratio: f32) -> Option<f32> {
    if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
        return None;
    }
    Some(ratio.clamp(0.0, 1.0))
}

fn authoritative_cursor_progress(progress: &PlaybackRuntimeProgress) -> Option<f32> {
    if !progress.active || progress.error.is_some() {
        return None;
    }
    preview_cursor_ratio(progress.progress?)
}

fn cursor_interpolation(
    from: f32,
    to: f32,
    started_at: Instant,
) -> Option<PlaybackCursorInterpolation> {
    if preview_cursor_ratio(from).is_none()
        || preview_cursor_ratio(to).is_none()
        || to + f32::EPSILON < from
        || (from - to).abs() <= f32::EPSILON
    {
        return None;
    }
    Some(PlaybackCursorInterpolation {
        from,
        to,
        started_at,
        duration: PLAYBACK_CURSOR_MAX_INTERPOLATION,
    })
}

fn playback_request_is_current(
    latest: Option<PlaybackRequestId>,
    event: PlaybackRequestId,
) -> bool {
    playback_request_number_is_current(latest.map(PlaybackRequestId::get), event.get())
}

fn playback_request_number_is_current(latest: Option<u64>, event: u64) -> bool {
    latest == Some(event)
}

fn progress_poll_is_current(pending: Option<PendingProgressPoll>, event: u64, epoch: u64) -> bool {
    pending.is_some_and(|poll| poll.request_id == event && poll.playback_epoch == epoch)
}

fn progress_poll_has_expired(pending: PendingProgressPoll, now: Instant) -> bool {
    now.saturating_duration_since(pending.sent_at) >= PLAYBACK_PROGRESS_POLL_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::{
        LoopRetargetProgressOutcome, PendingLoopRetarget, PendingProgressPoll,
        PlaybackCursorInterpolation, PlaybackIntent, PlaybackIntentOrigin,
        PlaybackProgressDisposition, PlaybackRetargetAcknowledgement, PlaybackRetargetAnchor,
        PlaybackRetargetRequest, accepted_resume_intent, admit_loop_retarget,
        apply_pending_loop_retarget_to_resume, audio_output_resume_is_current,
        authoritative_cursor_progress, captured_metronome_grid_or_source, cursor_interpolation,
        fallback_metronome_target, metronome_target_is_pending, normalized_playback_start,
        pending_resume_intent, playback_mode_for_intent, playback_progress_disposition,
        playback_request_number_is_current, playback_retarget_anchor_matches,
        playback_span_for_intent, playback_span_matches_preparation, preferred_metronome_grid,
        preview_cursor_ratio, progress_poll_has_expired, progress_poll_is_current,
        reconcile_playback_intent_mode, reduce_loop_retarget_progress,
        reduce_playback_retarget_progress, resolve_queued_playback_intent,
        retain_pending_loop_retarget_after_failed_admission, should_cancel_pending_autoplay,
        should_queue_initialized_playback, should_record_last_played,
        should_record_playback_history,
    };
    use reson::{PlaybackRuntimeMode, PlaybackRuntimeProgress};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::super::waveform::WaveformPlayRegion;
    use wavecrate_core::{AudioFile, PlaybackLoopPolicy, PlaybackType};

    fn sample_intent(path: &Path, looped: bool) -> PlaybackIntent {
        PlaybackIntent {
            start: 0.4,
            region: None,
            looped,
            volume: 1.0,
            path: path.to_path_buf(),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        }
    }

    fn sample_pending_retarget(
        path: &Path,
        retarget_id: u64,
        desired_looped: bool,
    ) -> PendingLoopRetarget {
        sample_pending_playback_retarget(
            path,
            retarget_id,
            super::PlaybackDesiredState {
                looped: desired_looped,
                metronome_beat_count: None,
            },
        )
    }

    fn sample_pending_playback_retarget(
        path: &Path,
        retarget_id: u64,
        desired: super::PlaybackDesiredState,
    ) -> PendingLoopRetarget {
        PendingLoopRetarget {
            retarget_id,
            request: PlaybackRetargetRequest {
                generation: retarget_id,
                target: super::PlaybackRetargetTarget {
                    span: WaveformPlayRegion::new(0.4, 1.0).expect("valid test span"),
                    desired,
                    seek_if_outside: false,
                },
                anchor: PlaybackRetargetAnchor {
                    playback_id: 42,
                    playback_epoch: 7,
                    waveform_generation: 0,
                    output_open_generation: 3,
                    path: path.to_path_buf(),
                },
            },
            metronome_grid: None,
            effective_metronome_beat_count: desired.metronome_beat_count,
        }
    }

    fn reduce_ack(
        pending: &mut Option<PendingLoopRetarget>,
        accepted: &mut Option<PlaybackIntent>,
        response_id: u64,
        looping: bool,
        metronome_beat_count: Option<u16>,
        acknowledgement: PlaybackRetargetAcknowledgement,
    ) -> LoopRetargetProgressOutcome {
        let path = Path::new("sample.wav");
        super::reduce_playback_retarget_progress(
            pending,
            accepted,
            response_id,
            Some(42),
            7,
            3,
            Some(path),
            Some(path),
            true,
            false,
            Some(42),
            looping,
            metronome_beat_count,
            acknowledgement,
        )
    }

    #[test]
    fn playback_retarget_anchor_rejects_every_lifecycle_change() {
        let path = PathBuf::from("sample.wav");
        let other_path = PathBuf::from("other.wav");
        let anchor = PlaybackRetargetAnchor {
            playback_id: 42,
            playback_epoch: 7,
            waveform_generation: 5,
            output_open_generation: 3,
            path: path.clone(),
        };
        let matches = |latest, epoch, waveform, output, playing, accepted| {
            playback_retarget_anchor_matches(
                &anchor, latest, epoch, waveform, output, playing, accepted,
            )
        };

        assert!(matches(
            Some(42),
            7,
            5,
            3,
            Some(path.as_path()),
            Some(path.as_path())
        ));
        for (latest, epoch, waveform, output, playing, accepted) in [
            (Some(43), 7, 5, 3, path.as_path(), path.as_path()),
            (Some(42), 8, 5, 3, path.as_path(), path.as_path()),
            (Some(42), 7, 6, 3, path.as_path(), path.as_path()),
            (Some(42), 7, 5, 4, path.as_path(), path.as_path()),
            (Some(42), 7, 5, 3, other_path.as_path(), path.as_path()),
            (Some(42), 7, 5, 3, path.as_path(), other_path.as_path()),
        ] {
            assert!(!matches(
                latest,
                epoch,
                waveform,
                output,
                Some(playing),
                Some(accepted)
            ));
        }
    }

    #[gpui::test]
    fn repeated_output_capture_recovers_fenced_normalized_resume(cx: &mut gpui::TestAppContext) {
        use super::super::normalized_audition::{
            NormalizedAuditionAction, NormalizedAuditionFence, PendingNormalizedAudition,
        };
        use gpui::AppContext;
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(super::Browser::new);
        browser.update(cx, |view, _| {
            let path = PathBuf::from("sample.wav");
            view.entries = vec![AudioFile {
                path: path.clone(),
                relative_path: path.clone(),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            view.normalized_audition.set_enabled(true);
            let accepted = sample_intent(&path, false);
            let desired = super::PlaybackDesiredState {
                looped: true,
                metronome_beat_count: Some(6),
            };
            let normalized_request = super::PlaybackRetargetRequest {
                generation: 1,
                target: super::PlaybackRetargetTarget {
                    span: playback_span_for_intent(&accepted).expect("accepted span"),
                    desired,
                    seek_if_outside: false,
                },
                anchor: super::PlaybackRetargetAnchor {
                    playback_id: 42,
                    playback_epoch: view.playback_epoch,
                    waveform_generation: view.waveform_generation,
                    output_open_generation: view.audio_output_open_generation,
                    path: path.clone(),
                },
            };
            let resume = super::capture_resume_configuration(
                &accepted,
                1.0,
                None,
                None,
                Some(&normalized_request),
            );
            let span = playback_span_for_intent(&resume).unwrap();
            let pending = PendingNormalizedAudition {
                fence: NormalizedAuditionFence::new(
                    view.playback_epoch,
                    view.waveform_generation,
                    view.audio_output_open_generation,
                    None,
                    path.clone(),
                    span,
                ),
                action: NormalizedAuditionAction::Play(resume),
            };
            view.normalized_audition_deferred_play = Some(pending.clone());
            let recaptured = view
                .capture_audio_output_resume_intent()
                .expect("resume during preparation");
            assert_eq!(recaptured.resume_desired, Some(desired));
            view.stop_playback();
            assert!(view.capture_audio_output_resume_intent().is_none());
            let mut next = pending.clone();
            next.fence.playback_epoch = view.playback_epoch;
            next.action = NormalizedAuditionAction::Play(recaptured);
            view.normalized_audition_queued = Some(next);
            assert_eq!(
                view.capture_audio_output_resume_intent()
                    .unwrap()
                    .resume_desired,
                Some(desired)
            );
            view.waveform_generation += 1;
            assert!(view.capture_audio_output_resume_intent().is_none());
        });
    }

    #[test]
    fn output_resume_keeps_newest_complete_target_through_repeated_capture() {
        let path = Path::new("sample.wav");
        let accepted = sample_intent(path, false);
        let pending = sample_pending_playback_retarget(
            path,
            44,
            super::PlaybackDesiredState {
                looped: true,
                metronome_beat_count: Some(4),
            },
        );
        let latest = super::PlaybackDesiredState {
            looped: false,
            metronome_beat_count: Some(7),
        };
        let latest_target = super::PlaybackRetargetTarget {
            span: WaveformPlayRegion::new(0.15, 0.65).expect("valid target span"),
            desired: latest,
            seek_if_outside: true,
        };
        let latest_request = super::PlaybackRetargetRequest {
            generation: 45,
            target: latest_target,
            anchor: super::PlaybackRetargetAnchor {
                playback_id: 42,
                playback_epoch: 7,
                waveform_generation: 0,
                output_open_generation: 3,
                path: path.to_path_buf(),
            },
        };
        let resume = super::capture_resume_configuration(
            &accepted,
            0.75,
            None,
            Some(&pending),
            Some(&latest_request),
        );
        assert_eq!(resume.resume_desired, Some(latest));
        assert!(!resume.looped);
        assert_eq!(resume.region, Some(latest_target.span));
        let repeated = pending_resume_intent(&resume, 0.5);
        assert_eq!(repeated.resume_desired, Some(latest));
        assert_eq!(super::metronome_count_for_intent(&repeated, None), Some(7));
        assert_eq!(repeated.volume, 0.5);
        let admitted =
            super::capture_resume_configuration(&accepted, 0.75, None, Some(&pending), None);
        assert_eq!(
            admitted.resume_desired,
            Some(pending.request.target.desired)
        );
        let confirmed = super::capture_resume_configuration(&accepted, 0.75, Some(3), None, None);
        assert_eq!(
            confirmed.resume_desired.unwrap().metronome_beat_count,
            Some(3)
        );
    }

    #[test]
    fn output_resume_retains_admitted_count_before_first_progress() {
        let mut accepted = sample_intent(Path::new("sample.wav"), false);
        accepted.resume_desired = Some(super::PlaybackDesiredState {
            looped: false,
            metronome_beat_count: Some(5),
        });
        let resume = super::capture_resume_configuration(&accepted, 1.0, None, None, None);
        assert_eq!(super::metronome_count_for_intent(&resume, None), Some(5));
        accepted
            .resume_desired
            .as_mut()
            .unwrap()
            .metronome_beat_count = None;
        let disabled = super::capture_resume_configuration(&accepted, 1.0, None, None, None);
        assert_eq!(super::metronome_count_for_intent(&disabled, Some(5)), None);
    }

    #[test]
    fn disabled_resume_metronome_does_not_fall_back_to_idle_preference() {
        let accepted = sample_intent(Path::new("sample.wav"), false);
        let resume = super::capture_resume_configuration(&accepted, 1.0, None, None, None);
        assert_eq!(super::metronome_count_for_intent(&resume, Some(8)), None);
        assert_eq!(
            super::metronome_count_for_intent(&accepted, Some(8)),
            Some(8)
        );
    }

    #[test]
    fn stale_stop_event_cannot_clear_a_replay() {
        let first_play = 1;
        let stop = 2;
        let replay = 3;
        let latest = Some(replay);

        assert!(!playback_request_number_is_current(latest, first_play));
        assert!(!playback_request_number_is_current(latest, stop));
        assert!(playback_request_number_is_current(latest, replay));
    }

    #[test]
    fn stop_during_audio_init_cancels_pending_autoplay_intent() {
        let path = Path::new("sample.wav");

        assert!(should_queue_initialized_playback(
            4,
            8,
            4,
            8,
            Some(path),
            path
        ));
        assert!(!should_queue_initialized_playback(
            4,
            8,
            4,
            9,
            Some(path),
            path
        ));
        assert!(should_queue_initialized_playback(
            4,
            9,
            4,
            9,
            Some(path),
            path
        ));
    }

    #[test]
    fn output_reconfigure_preserves_resume_only_for_current_selection_and_epoch() {
        let path = Path::new("sample.wav");
        assert!(audio_output_resume_is_current(9, 4, 9, 4, Some(path), path,));
        assert!(!audio_output_resume_is_current(
            9,
            4,
            10,
            4,
            Some(path),
            path,
        ));
        assert!(!audio_output_resume_is_current(
            9,
            4,
            9,
            4,
            Some(Path::new("other.wav")),
            path,
        ));
    }

    #[test]
    fn disabling_autoplay_only_cancels_selection_origin() {
        assert!(should_cancel_pending_autoplay(
            false,
            Some(PlaybackIntentOrigin::SelectionAutoplay)
        ));
        assert!(!should_cancel_pending_autoplay(
            false,
            Some(PlaybackIntentOrigin::Explicit)
        ));
        assert!(!should_cancel_pending_autoplay(
            true,
            Some(PlaybackIntentOrigin::SelectionAutoplay)
        ));
        assert!(!should_cancel_pending_autoplay(false, None));
    }

    #[test]
    fn pending_init_origins_record_but_accepted_output_resume_does_not() {
        assert!(should_record_playback_history(
            PlaybackIntentOrigin::Explicit
        ));
        assert!(should_record_playback_history(
            PlaybackIntentOrigin::SelectionAutoplay
        ));
        assert!(!should_record_playback_history(
            PlaybackIntentOrigin::HistoryReplay
        ));
        assert!(!should_record_playback_history(
            PlaybackIntentOrigin::AudioOutputResume
        ));
    }

    #[test]
    fn accepted_playback_origins_record_last_played_but_output_resume_does_not() {
        assert!(should_record_last_played(PlaybackIntentOrigin::Explicit));
        assert!(should_record_last_played(
            PlaybackIntentOrigin::SelectionAutoplay
        ));
        assert!(should_record_last_played(
            PlaybackIntentOrigin::HistoryReplay
        ));
        assert!(!should_record_last_played(
            PlaybackIntentOrigin::AudioOutputResume
        ));
    }

    #[test]
    fn queue_resolution_requires_the_loaded_path_and_applies_its_tagged_mode() {
        let path = PathBuf::from("sample.wav");
        let entry = AudioFile {
            path: path.clone(),
            relative_path: PathBuf::from("sample.wav"),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: Some(PlaybackType::Loop),
            curation: None,
        };
        let intent = PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 1.0,
            path: path.clone(),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        let mut policy = PlaybackLoopPolicy::default();
        let resolved = resolve_queued_playback_intent(&mut policy, intent, Some(&entry))
            .expect("the loaded path should resolve");
        assert!(resolved.looped);
        assert_eq!(policy.current_file(), Some(&entry.stable_id()));

        policy.set_manual(false);
        let manual_intent = PlaybackIntent {
            looped: true,
            path: path.clone(),
            ..resolved.clone()
        };
        let resolved_manual =
            resolve_queued_playback_intent(&mut policy, manual_intent, Some(&entry))
                .expect("the loaded path should keep resolving");
        assert!(!resolved_manual.looped);

        let stale = PlaybackIntent {
            path: PathBuf::from("other.wav"),
            ..resolved
        };
        assert!(resolve_queued_playback_intent(&mut policy, stale, Some(&entry)).is_none());

        let resume = PlaybackIntent {
            origin: PlaybackIntentOrigin::AudioOutputResume,
            resume_desired: None,
            looped: true,
            path,
            ..resolved_manual
        };
        let captured = resolve_queued_playback_intent(&mut policy, resume, Some(&entry))
            .expect("the loaded path should keep resolving");
        assert!(captured.looped);
    }

    #[test]
    fn accepted_resume_keeps_mode_and_region_but_uses_current_volume() {
        let region = WaveformPlayRegion::new(0.2, 0.8).expect("valid region");
        let accepted = PlaybackIntent {
            start: region.start,
            region: Some(region),
            looped: true,
            volume: 0.25,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::HistoryReplay,
            resume_desired: None,
        };
        let resume = accepted_resume_intent(&accepted, 0.75);
        assert_eq!(resume.origin, PlaybackIntentOrigin::AudioOutputResume);
        assert!(resume.looped);
        assert_eq!(resume.region, accepted.region);
        assert_eq!(resume.start, accepted.start);
        assert_eq!(resume.path, accepted.path);
        assert_eq!(resume.volume, 0.75);
    }

    #[test]
    fn pending_output_resume_keeps_origin_and_refreshes_volume() {
        let pending = PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 0.25,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::SelectionAutoplay,
            resume_desired: None,
        };
        let resume = pending_resume_intent(&pending, 0.75);
        assert_eq!(resume.origin, PlaybackIntentOrigin::SelectionAutoplay);
        assert_eq!(resume.volume, 0.75);
        assert_eq!(resume.path, pending.path);
    }

    #[test]
    fn playback_mode_preserves_click_offset_and_loop_preference() {
        let path = PathBuf::from("sample.wav");
        assert_eq!(
            playback_mode_for_intent(&PlaybackIntent {
                start: 1.2,
                region: None,
                looped: false,
                volume: 1.0,
                path: path.clone(),
                origin: PlaybackIntentOrigin::Explicit,
                resume_desired: None,
            }),
            PlaybackRuntimeMode::OneShot {
                start: 1.0,
                end: 1.0,
            }
        );
        assert_eq!(
            playback_mode_for_intent(&PlaybackIntent {
                start: 0.35,
                region: None,
                looped: true,
                volume: 1.0,
                path,
                origin: PlaybackIntentOrigin::SelectionAutoplay,
                resume_desired: None,
            }),
            PlaybackRuntimeMode::Looped {
                start: 0.0,
                end: 1.0,
                offset: 0.35,
            }
        );
    }

    #[test]
    fn playback_mode_uses_the_selected_region_for_one_shot_and_loop() {
        let path = PathBuf::from("sample.wav");
        let region = WaveformPlayRegion::new(0.25, 0.75).expect("valid region");
        let one_shot = PlaybackIntent {
            start: region.start,
            region: Some(region),
            looped: false,
            volume: 1.0,
            path: path.clone(),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        assert_eq!(
            playback_mode_for_intent(&one_shot),
            PlaybackRuntimeMode::OneShot {
                start: 0.25,
                end: 0.75,
            }
        );

        let looped = PlaybackIntent {
            looped: true,
            ..one_shot
        };
        assert_eq!(
            playback_mode_for_intent(&looped),
            PlaybackRuntimeMode::Looped {
                start: 0.25,
                end: 0.75,
                offset: 0.25,
            }
        );
    }

    #[test]
    fn output_resume_prefers_the_admitted_grid_before_current_waveform_metadata() {
        let path = Path::new("sample.wav");
        let captured = super::PlaybackMetronomeGrid {
            frames: 48_000,
            origin_frame: 12_000,
        };
        let current = super::PlaybackMetronomeGrid {
            frames: 96_000,
            origin_frame: 24_000,
        };
        let mut source_called = false;
        let selected = preferred_metronome_grid(
            PlaybackIntentOrigin::AudioOutputResume,
            path,
            Some(path),
            Some(captured),
            || {
                source_called = true;
                Some(current)
            },
        );
        assert_eq!(selected, Some(captured));
        assert!(!source_called);

        let mut source_called = false;
        let fallback = preferred_metronome_grid(
            PlaybackIntentOrigin::AudioOutputResume,
            path,
            Some(Path::new("other.wav")),
            Some(captured),
            || {
                source_called = true;
                Some(current)
            },
        );
        assert_eq!(fallback, Some(current));
        assert!(source_called);
    }

    #[test]
    fn an_admitted_missing_grid_stays_missing_for_live_retargets() {
        let path = Path::new("sample.wav");
        let current = super::PlaybackMetronomeGrid {
            frames: 96_000,
            origin_frame: 24_000,
        };
        let mut source_called = false;
        let selected = captured_metronome_grid_or_source(path, Some(path), None, || {
            source_called = true;
            Some(current)
        });
        assert_eq!(selected, None);
        assert!(!source_called);

        let mut source_called = false;
        let fallback =
            captured_metronome_grid_or_source(path, Some(Path::new("other.wav")), None, || {
                source_called = true;
                Some(current)
            });
        assert_eq!(fallback, Some(current));
        assert!(source_called);
    }

    #[test]
    fn metronome_pending_state_separates_unconfirmed_enable_from_loop_only_updates() {
        assert!(metronome_target_is_pending(Some(4), None, false));
        assert!(!metronome_target_is_pending(None, None, false));
        assert!(!metronome_target_is_pending(Some(4), Some(4), true));
        assert!(metronome_target_is_pending(None, Some(4), true));
    }

    #[test]
    fn failed_retarget_falls_back_to_confirmed_disabled_metronome() {
        let failed_enable = super::PlaybackDesiredState {
            looped: false,
            metronome_beat_count: Some(8),
        };
        assert_eq!(
            fallback_metronome_target(None, true, Some(failed_enable), true, 8),
            None
        );
        assert_eq!(
            fallback_metronome_target(Some(4), true, Some(failed_enable), false, 8),
            Some(4)
        );
        assert_eq!(
            fallback_metronome_target(None, false, Some(failed_enable), false, 8),
            Some(8)
        );
    }

    #[test]
    fn pending_playback_intent_tracks_the_latest_quantized_volume() {
        let mut intent = PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 1.0,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        intent.set_volume(0.7346);
        assert_eq!(intent.volume, 0.735);
    }

    #[test]
    fn playback_start_clamps_finite_positions_and_rejects_non_finite_input() {
        assert_eq!(normalized_playback_start(-0.2), Some(0.0));
        assert_eq!(normalized_playback_start(1.2), Some(1.0));
        assert_eq!(normalized_playback_start(f64::NAN), None);
        assert_eq!(normalized_playback_start(f64::INFINITY), None);
    }

    #[test]
    fn normalized_progress_maps_to_the_bounded_preview_with_edge_clamp() {
        assert_eq!(preview_cursor_ratio(0.0), Some(0.0));
        assert_eq!(preview_cursor_ratio(0.5), Some(0.5));
        assert_eq!(preview_cursor_ratio(1.0), Some(1.0));
    }

    #[test]
    fn invalid_cursor_mapping_inputs_are_hidden_instead_of_clamped() {
        assert_eq!(preview_cursor_ratio(-0.5), None);
        assert_eq!(preview_cursor_ratio(1.000_1), None);
        assert_eq!(preview_cursor_ratio(f32::NAN), None);
        assert_eq!(preview_cursor_ratio(f32::INFINITY), None);
    }

    #[test]
    fn interpolation_stays_between_engine_samples_and_settles_at_the_target() {
        let started_at = Instant::now();
        let interpolation = PlaybackCursorInterpolation {
            from: 0.2,
            to: 0.4,
            started_at,
            duration: Duration::from_millis(80),
        };

        assert_eq!(interpolation.value_at(started_at), 0.2);
        let middle = interpolation.value_at(started_at + Duration::from_millis(40));
        assert!(middle > 0.2 && middle < 0.4);
        assert_eq!(
            interpolation.value_at(started_at + Duration::from_millis(200)),
            0.4
        );
        assert!(!interpolation.needs_sample_at(started_at + Duration::from_millis(80)));
    }

    #[test]
    fn cursor_lifecycle_requires_active_error_free_normalized_progress() {
        let active = PlaybackRuntimeProgress {
            active: true,
            progress: Some(0.25),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_progress(&active), Some(0.25));

        let completed = PlaybackRuntimeProgress {
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_progress(&completed), None);

        let missing_progress = PlaybackRuntimeProgress {
            active: true,
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_progress(&missing_progress), None);

        let looping = PlaybackRuntimeProgress {
            active: true,
            looping: true,
            progress: Some(0.25),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_progress(&looping), Some(0.25));

        let errored = PlaybackRuntimeProgress {
            active: true,
            progress: Some(0.25),
            error: Some("output failed".to_string()),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_progress(&errored), None);
    }

    #[test]
    fn loop_wrap_snaps_without_a_backwards_cursor_sweep() {
        let started_at = Instant::now();

        assert!(cursor_interpolation(0.9, 0.1, started_at).is_none());
        assert!(cursor_interpolation(0.2, 0.4, started_at).is_some());
    }

    #[test]
    fn live_loop_span_keeps_a_seeked_one_shot_range() {
        let path = PathBuf::from("sample.wav");
        let one_shot = PlaybackIntent {
            start: 0.4,
            region: None,
            looped: false,
            volume: 1.0,
            path: path.clone(),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        let span = playback_span_for_intent(&one_shot).expect("seeked one-shot has a span");
        assert_eq!(span, WaveformPlayRegion::new(0.4, 1.0).unwrap());

        let confirmed_loop = PlaybackIntent {
            region: Some(span),
            looped: true,
            ..one_shot
        };
        assert_eq!(
            playback_mode_for_intent(&confirmed_loop),
            PlaybackRuntimeMode::Looped {
                start: 0.4,
                end: 1.0,
                offset: 0.4,
            }
        );
        assert_eq!(playback_span_for_intent(&confirmed_loop), Some(span));
    }

    #[test]
    fn ordinary_progress_converges_mode_without_expanding_a_seeked_span() {
        let mut intent = PlaybackIntent {
            start: 0.4,
            region: None,
            looped: false,
            volume: 1.0,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        reconcile_playback_intent_mode(Some(&mut intent), true);
        assert_eq!(intent.region, WaveformPlayRegion::new(0.4, 1.0));
        assert!(intent.looped);

        reconcile_playback_intent_mode(Some(&mut intent), false);
        assert_eq!(
            playback_mode_for_intent(&intent),
            PlaybackRuntimeMode::OneShot {
                start: 0.4,
                end: 1.0,
            }
        );
    }

    #[test]
    fn unchanged_loop_progress_preserves_the_accepted_resume_offset() {
        let mut intent = PlaybackIntent {
            start: 0.4,
            region: None,
            looped: true,
            volume: 1.0,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        reconcile_playback_intent_mode(Some(&mut intent), true);
        let resume = accepted_resume_intent(&intent, 0.75);
        assert_eq!(
            playback_mode_for_intent(&resume),
            PlaybackRuntimeMode::Looped {
                start: 0.0,
                end: 1.0,
                offset: 0.4,
            }
        );
        assert_eq!(resume.volume, 0.75);
    }

    #[test]
    fn prepared_normalized_span_must_match_resolved_loop_mode() {
        let mut intent = sample_intent(Path::new("sample.wav"), false);
        let full_track = WaveformPlayRegion::new(0.0, 1.0).expect("valid span");
        assert!(!playback_span_matches_preparation(&intent, full_track));
        intent.looped = true;
        assert!(playback_span_matches_preparation(&intent, full_track));
    }

    #[test]
    fn live_loop_reducer_ignores_old_ack_and_confirms_from_newer_poll() {
        let path = PathBuf::from("sample.wav");
        let span = WaveformPlayRegion::new(0.4, 1.0).expect("valid test span");
        let mut accepted = Some(sample_intent(&path, false));
        let first = sample_pending_retarget(&path, 11, true);
        let second = sample_pending_retarget(&path, 12, false);
        let mut pending = None;
        admit_loop_retarget(&mut pending, first);
        admit_loop_retarget(&mut pending, second.clone());

        let old_ack = reduce_loop_retarget_progress(
            &mut pending,
            &mut accepted,
            11,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            false,
            Some(42),
            true,
        );
        assert_eq!(old_ack, LoopRetargetProgressOutcome::Ignored);
        assert_eq!(pending, Some(second.clone()));
        assert!(!accepted.as_ref().expect("accepted play").looped);

        let newer_poll = reduce_loop_retarget_progress(
            &mut pending,
            &mut accepted,
            13,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            false,
            Some(42),
            false,
        );
        assert_eq!(
            newer_poll,
            LoopRetargetProgressOutcome::Confirmed {
                desired_matches: true,
            }
        );
        assert!(pending.is_none());
        let accepted = accepted.expect("accepted play");
        assert!(!accepted.looped);
        assert_eq!(accepted.region, Some(span));
    }

    #[test]
    fn playback_reducer_requires_the_complete_loop_and_metronome_tuple() {
        let path = PathBuf::from("sample.wav");
        let desired = super::PlaybackDesiredState {
            looped: true,
            metronome_beat_count: Some(8),
        };
        let mut accepted = Some(sample_intent(&path, false));
        let mut pending = Some(sample_pending_playback_retarget(&path, 11, desired));

        let partial = reduce_playback_retarget_progress(
            &mut pending,
            &mut accepted,
            11,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            false,
            Some(42),
            true,
            Some(4),
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(11),
                playback_span_frames: Some((40, 100)),
                track_total_frames: Some(100),
            },
        );
        assert_eq!(partial, super::LoopRetargetProgressOutcome::Ignored);
        assert!(pending.is_some());
        assert!(!accepted.as_ref().expect("accepted play").looped);

        let mut accepted = Some(sample_intent(&path, false));
        let mut pending = Some(sample_pending_playback_retarget(&path, 12, desired));
        let confirmed = reduce_playback_retarget_progress(
            &mut pending,
            &mut accepted,
            12,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            false,
            Some(42),
            true,
            Some(8),
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(12),
                playback_span_frames: Some((40, 100)),
                track_total_frames: Some(100),
            },
        );
        assert_eq!(
            confirmed,
            super::LoopRetargetProgressOutcome::Confirmed {
                desired_matches: true,
            }
        );
        assert!(pending.is_none());
    }

    #[test]
    fn playback_reducer_requires_the_runtime_applied_plan_before_promoting_state() {
        let path = PathBuf::from("sample.wav");
        let desired = super::PlaybackDesiredState {
            looped: true,
            metronome_beat_count: Some(8),
        };
        let invalid_acknowledgements = [
            PlaybackRetargetAcknowledgement::default(),
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(10),
                playback_span_frames: Some((20, 80)),
                track_total_frames: Some(100),
            },
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(11),
                playback_span_frames: None,
                track_total_frames: Some(100),
            },
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(11),
                playback_span_frames: Some((80, 20)),
                track_total_frames: Some(100),
            },
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(11),
                playback_span_frames: Some((20, 101)),
                track_total_frames: Some(100),
            },
        ];
        for acknowledgement in invalid_acknowledgements {
            let mut accepted = Some(sample_intent(&path, false));
            let accepted_before = accepted.clone();
            let mut pending = Some(sample_pending_playback_retarget(&path, 11, desired));
            assert_eq!(
                reduce_ack(
                    &mut pending,
                    &mut accepted,
                    12,
                    true,
                    Some(8),
                    acknowledgement,
                ),
                LoopRetargetProgressOutcome::Ignored
            );
            assert!(pending.is_some());
            assert_eq!(accepted, accepted_before);
        }

        for (looping, metronome_beat_count) in [(false, Some(8)), (true, Some(4))] {
            let mut accepted = Some(sample_intent(&path, false));
            let accepted_before = accepted.clone();
            let mut pending = Some(sample_pending_playback_retarget(&path, 11, desired));
            assert_eq!(
                reduce_ack(
                    &mut pending,
                    &mut accepted,
                    12,
                    looping,
                    metronome_beat_count,
                    PlaybackRetargetAcknowledgement {
                        configured_span_request_id: Some(11),
                        playback_span_frames: Some((20, 80)),
                        track_total_frames: Some(100),
                    },
                ),
                LoopRetargetProgressOutcome::Ignored
            );
            assert!(pending.is_some());
            assert_eq!(accepted, accepted_before);
        }

        let mut accepted = Some(sample_intent(&path, false));
        let mut pending = Some(sample_pending_playback_retarget(&path, 11, desired));
        assert_eq!(
            reduce_ack(
                &mut pending,
                &mut accepted,
                12,
                true,
                Some(8),
                PlaybackRetargetAcknowledgement {
                    configured_span_request_id: Some(11),
                    playback_span_frames: Some((20, 80)),
                    track_total_frames: Some(100),
                },
            ),
            LoopRetargetProgressOutcome::Confirmed {
                desired_matches: true,
            }
        );
        assert!(pending.is_none());
        assert_eq!(
            accepted.expect("accepted playback").region,
            WaveformPlayRegion::new(0.2, 0.8)
        );
    }

    #[test]
    fn playback_reducer_accepts_disabled_effective_metronome_without_waiting() {
        let path = PathBuf::from("sample.wav");
        let desired = super::PlaybackDesiredState {
            looped: true,
            metronome_beat_count: Some(8),
        };
        let mut pending = sample_pending_playback_retarget(&path, 11, desired);
        pending.effective_metronome_beat_count = None;
        let mut pending = Some(pending);
        let mut accepted = Some(sample_intent(&path, false));
        assert_eq!(
            reduce_ack(
                &mut pending,
                &mut accepted,
                11,
                true,
                None,
                PlaybackRetargetAcknowledgement {
                    configured_span_request_id: Some(11),
                    playback_span_frames: Some((40, 100)),
                    track_total_frames: Some(100),
                },
            ),
            LoopRetargetProgressOutcome::Confirmed {
                desired_matches: false,
            }
        );
        assert!(pending.is_none());
        assert_eq!(
            accepted.expect("accepted playback").region,
            WaveformPlayRegion::new(0.4, 1.0)
        );
    }

    #[test]
    fn playback_reducer_ignores_an_old_tuple_ack_after_a_newer_target() {
        let path = PathBuf::from("sample.wav");
        let first = super::PlaybackDesiredState {
            looped: true,
            metronome_beat_count: Some(4),
        };
        let newest = super::PlaybackDesiredState {
            looped: false,
            metronome_beat_count: Some(8),
        };
        let mut accepted = Some(sample_intent(&path, false));
        let mut pending = None;
        admit_loop_retarget(
            &mut pending,
            sample_pending_playback_retarget(&path, 11, first),
        );
        admit_loop_retarget(
            &mut pending,
            sample_pending_playback_retarget(&path, 12, newest),
        );

        let stale_failure = reduce_playback_retarget_progress(
            &mut pending,
            &mut accepted,
            11,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            true,
            Some(42),
            true,
            Some(4),
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(11),
                playback_span_frames: Some((40, 100)),
                track_total_frames: Some(100),
            },
        );
        assert_eq!(stale_failure, super::LoopRetargetProgressOutcome::Ignored);
        assert_eq!(
            pending
                .as_ref()
                .map(|pending| pending.request.target.desired),
            Some(newest)
        );
        assert!(!accepted.as_ref().expect("accepted play").looped);

        let confirmed = reduce_playback_retarget_progress(
            &mut pending,
            &mut accepted,
            13,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
            true,
            false,
            Some(42),
            false,
            Some(8),
            PlaybackRetargetAcknowledgement {
                configured_span_request_id: Some(12),
                playback_span_frames: Some((40, 100)),
                track_total_frames: Some(100),
            },
        );
        assert_eq!(
            confirmed,
            super::LoopRetargetProgressOutcome::Confirmed {
                desired_matches: true,
            }
        );
        assert!(pending.is_none());
        assert!(!accepted.as_ref().expect("accepted play").looped);
    }

    #[test]
    fn failed_live_loop_admission_retains_matching_pending_command() {
        let path = PathBuf::from("sample.wav");
        let first = sample_pending_retarget(&path, 11, true);
        let mut pending = Some(first.clone());

        assert!(retain_pending_loop_retarget_after_failed_admission(
            &mut pending,
            Some(42),
            7,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
        ));
        assert_eq!(pending, Some(first));

        assert!(!retain_pending_loop_retarget_after_failed_admission(
            &mut pending,
            Some(42),
            8,
            3,
            Some(path.as_path()),
            Some(path.as_path()),
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn stale_live_loop_fences_and_active_identity_do_not_mutate_pending_state() {
        let path = PathBuf::from("sample.wav");
        let other_path = PathBuf::from("other.wav");
        let mut pending = Some(sample_pending_retarget(&path, 11, true));
        let mut accepted = Some(sample_intent(&path, false));

        let stale_fences = [
            (Some(43), 7, 3, path.as_path(), path.as_path()),
            (Some(42), 8, 3, path.as_path(), path.as_path()),
            (Some(42), 7, 4, path.as_path(), path.as_path()),
            (Some(42), 7, 3, other_path.as_path(), path.as_path()),
            (Some(42), 7, 3, path.as_path(), other_path.as_path()),
        ];
        for (latest, epoch, generation, playing_path, accepted_path) in stale_fences {
            let outcome = reduce_loop_retarget_progress(
                &mut pending,
                &mut accepted,
                11,
                latest,
                epoch,
                generation,
                Some(playing_path),
                Some(accepted_path),
                true,
                false,
                Some(42),
                true,
            );
            assert_eq!(outcome, LoopRetargetProgressOutcome::Ignored);
            assert_eq!(
                pending.as_ref().map(|pending| pending.retarget_id),
                Some(11)
            );
            assert!(!accepted.as_ref().expect("accepted play").looped);
        }

        for has_error in [false, true] {
            let stale_play = reduce_loop_retarget_progress(
                &mut pending,
                &mut accepted,
                11,
                Some(42),
                7,
                3,
                Some(path.as_path()),
                Some(path.as_path()),
                true,
                has_error,
                Some(99),
                true,
            );
            assert_eq!(stale_play, LoopRetargetProgressOutcome::Ignored);
            assert!(pending.is_some());
            assert!(!accepted.as_ref().expect("accepted play").looped);
        }

        assert_eq!(
            reduce_loop_retarget_progress(
                &mut pending,
                &mut accepted,
                11,
                Some(42),
                7,
                3,
                Some(path.as_path()),
                Some(path.as_path()),
                true,
                false,
                Some(42),
                true,
            ),
            LoopRetargetProgressOutcome::Confirmed {
                desired_matches: true,
            }
        );
    }

    #[test]
    fn pending_output_resume_carries_desired_span_and_current_volume() {
        let path = PathBuf::from("sample.wav");
        let accepted = sample_intent(&path, false);
        let pending = sample_pending_retarget(&path, 11, true);
        let mut resume = accepted_resume_intent(&accepted, 0.735);

        apply_pending_loop_retarget_to_resume(&mut resume, Some(&pending), true);

        assert!(resume.looped);
        assert_eq!(resume.region, Some(pending.request.target.span));
        assert_eq!(resume.volume, 0.735);
        assert_eq!(resume.origin, PlaybackIntentOrigin::AudioOutputResume);
    }

    #[test]
    fn failed_and_inactive_live_loop_progress_preserve_confirmed_state() {
        let path = PathBuf::from("sample.wav");
        let mut accepted = Some(sample_intent(&path, false));
        let mut pending = Some(sample_pending_retarget(&path, 11, true));

        assert_eq!(
            reduce_loop_retarget_progress(
                &mut pending,
                &mut accepted,
                11,
                Some(42),
                7,
                3,
                Some(path.as_path()),
                Some(path.as_path()),
                true,
                true,
                Some(42),
                true,
            ),
            LoopRetargetProgressOutcome::Failed
        );
        assert!(pending.is_none());
        assert!(!accepted.as_ref().expect("accepted play").looped);

        pending = Some(sample_pending_retarget(&path, 12, true));
        assert_eq!(
            reduce_loop_retarget_progress(
                &mut pending,
                &mut accepted,
                12,
                Some(42),
                7,
                3,
                Some(path.as_path()),
                Some(path.as_path()),
                false,
                false,
                None,
                false,
            ),
            LoopRetargetProgressOutcome::Inactive
        );
        assert!(pending.is_none());
        assert!(!accepted.as_ref().expect("accepted play").looped);
    }

    #[test]
    fn progress_error_precedes_missing_identity_and_terminal_cleanup() {
        let failed = PlaybackRuntimeProgress {
            active: false,
            playback_request_id: None,
            error: Some("decoder failed".to_string()),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(
            playback_progress_disposition(
                Some(42),
                failed.playback_request_id.map(|id| id.get()),
                failed.error.is_some()
            ),
            PlaybackProgressDisposition::Error
        );

        let terminal = PlaybackRuntimeProgress {
            active: false,
            playback_request_id: None,
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(
            playback_progress_disposition(
                Some(42),
                terminal.playback_request_id.map(|id| id.get()),
                terminal.error.is_some()
            ),
            PlaybackProgressDisposition::TerminalWithoutIdentity
        );

        let unconfirmed_active = PlaybackRuntimeProgress {
            active: true,
            playback_request_id: None,
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(
            playback_progress_disposition(
                Some(42),
                unconfirmed_active.playback_request_id.map(|id| id.get()),
                unconfirmed_active.error.is_some()
            ),
            PlaybackProgressDisposition::TerminalWithoutIdentity
        );
    }

    #[test]
    fn stale_progress_errors_cannot_clear_current_playback() {
        assert_eq!(
            playback_progress_disposition(Some(42), Some(42), false),
            PlaybackProgressDisposition::Current
        );
        assert_eq!(
            playback_progress_disposition(Some(42), Some(41), true),
            PlaybackProgressDisposition::Stale
        );
        assert_eq!(
            playback_progress_disposition(Some(42), Some(42), true),
            PlaybackProgressDisposition::Error
        );
    }

    #[test]
    fn progress_poll_timeout_releases_late_responses() {
        let now = Instant::now();
        let pending = PendingProgressPoll {
            request_id: 9,
            playback_epoch: 4,
            sent_at: now - Duration::from_millis(399),
        };
        assert!(!progress_poll_has_expired(pending, now));
        assert!(progress_poll_is_current(Some(pending), 9, 4));
        assert!(!progress_poll_is_current(Some(pending), 8, 4));
        assert!(!progress_poll_is_current(Some(pending), 9, 5));

        let expired = PendingProgressPoll {
            sent_at: now - Duration::from_millis(400),
            ..pending
        };
        assert!(progress_poll_has_expired(expired, now));
        assert!(!progress_poll_is_current(None, 9, 4));
    }
}
