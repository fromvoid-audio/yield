#![deny(missing_docs)]
#![deny(warnings)]

//! Audio playback lifecycle for the browser.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Browser;
use super::waveform::WaveformPlayRegion;
use gpui::Context;
use reson::{
    PlaybackRequestId, PlaybackRuntimeEvent, PlaybackRuntimeMode, PlaybackRuntimeProgress,
    PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest, PlaybackRuntimeSource,
    PlaybackRuntimeStarted, PlaybackRuntimeStreamPolicy,
};
use wavecrate_core::AudioFile;
use wavecrate_core::PlaybackLoopPolicy;

const PLAYBACK_MONITOR_INTERVAL: Duration = Duration::from_millis(80);
const PLAYBACK_CURSOR_MAX_INTERPOLATION: Duration = PLAYBACK_MONITOR_INTERVAL;

/// One authoritative normalized playback sample retained by the browser cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaybackCursorSample {
    pub(super) ratio: f32,
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
    fn ensure_playback_monitor(&mut self, cx: &mut Context<Self>) {
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
                        );
                        view.poll_playback_events();
                        let changed = before
                            != (
                                view.playing_path.clone(),
                                view.playback_status.clone(),
                                view.playback_cursor_sample.map(|sample| sample.ratio),
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
        self.playing_path.is_some() || self.pending_progress_poll.is_some()
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
            .map(|intent| intent.origin);
        if !should_cancel_pending_autoplay(self.autoplay_selection, origin) {
            return false;
        }
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
        let enabled = !self.playback_policy.enabled();
        if !self.persist_loop_preference(enabled, cx) {
            return;
        }
        self.playback_policy.set_manual(enabled);
        if let Some(intent) = self.playback_init_intent.as_mut()
            && intent.origin != PlaybackIntentOrigin::AudioOutputResume
        {
            intent.looped = self.playback_policy.enabled();
        }
        if let Some(intent) = self.audio_output_resume.as_mut()
            && intent.origin != PlaybackIntentOrigin::AudioOutputResume
        {
            intent.looped = self.playback_policy.enabled();
        }
        self.playback_status = if self.playing_path.is_some() {
            format!(
                "Loop {} — applies on next play",
                if self.playback_policy.enabled() {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        } else {
            format!(
                "Loop {}",
                if self.playback_policy.enabled() {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        };
        cx.notify();
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
        })
    }

    pub(super) fn request_playback_intent(
        &mut self,
        intent: PlaybackIntent,
        cx: &mut Context<Self>,
    ) {
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
        let Some(start) = normalized_playback_start(intent.start) else {
            self.clear_playback_cursor();
            self.playback_status = "Invalid playback position".to_string();
            return;
        };
        let mode = playback_mode_for_intent(&intent);
        let request = PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile {
                path: intent.path.clone(),
            },
            mode,
            stream_policy: PlaybackRuntimeStreamPolicy::transient_navigation(),
            volume: super::volume::quantize_volume(intent.volume),
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: None,
            metronome: None,
        };
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
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
                self.accepted_playback_intent = Some(intent.clone());
                self.playing_path = Some(intent.path);
                self.playback_cursor_pending = Some(start as f32);
                self.playback_status = "Starting…".to_string();
                if record_last_played {
                    self.record_playback_for_intent(accepted_path.as_path(), accepted_at, cx);
                }
                self.ensure_playback_monitor(cx);
            }
            Err(error) => {
                self.latest_playback_request_id = None;
                self.playing_path = None;
                self.accepted_playback_intent = None;
                self.clear_playback_cursor();
                self.playback_status = format!("Could not queue playback: {error:?}");
            }
        }
        cx.notify();
    }

    pub(super) fn stop_playback(&mut self) {
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
        self.playback_monitor_generation = self.playback_monitor_generation.wrapping_add(1);
        self.playback_monitor_running = false;
        self.pending_progress_poll = None;
        self.playback_init_intent = None;
        self.playback_waiting_for_preferences = false;
        self.audio_output_resume = None;
        self.accepted_playback_intent = None;
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
                    self.playback_status = "Playing".to_string();
                }
                PlaybackRuntimeEvent::Progress { id, progress }
                    if progress_poll_is_current(
                        self.pending_progress_poll,
                        id,
                        self.playback_epoch,
                    ) =>
                {
                    self.pending_progress_poll = None;
                    if let Some(error) = progress.error {
                        self.clear_playback_cursor();
                        self.latest_playback_request_id = None;
                        self.playing_path = None;
                        self.accepted_playback_intent = None;
                        self.playback_status = format!("Playback failed: {error}");
                    } else if progress.active {
                        self.record_playback_progress(&progress);
                        if let Some(progress) = progress.progress {
                            let percent = progress * 100.0;
                            self.playback_status = format!("Playing {percent:.0}%");
                        } else {
                            self.playback_status = "Playing".to_string();
                        }
                    } else {
                        self.clear_playback_cursor();
                        self.latest_playback_request_id = None;
                        self.playing_path = None;
                        self.accepted_playback_intent = None;
                        self.playback_status = "Stopped".to_string();
                    }
                }
                PlaybackRuntimeEvent::Stopped { id }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
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
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.accepted_playback_intent = None;
                    self.playback_status = format!("Playback failed: {error}");
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
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
                    self.pending_progress_poll = Some((request_id, self.playback_epoch));
                }
            }
        }
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
        let playing_path = self.playing_path.as_ref()?;
        if selected_path != Some(playing_path) {
            return None;
        }
        let accepted = self.accepted_playback_intent.as_ref()?;
        if accepted.path != *playing_path {
            return None;
        }
        Some(accepted_resume_intent(accepted, self.volume))
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

fn progress_poll_is_current(
    pending: Option<(PlaybackRequestId, u64)>,
    event: PlaybackRequestId,
    epoch: u64,
) -> bool {
    pending.is_some_and(|(poll, pending_epoch)| poll.get() == event.get() && pending_epoch == epoch)
}

#[cfg(test)]
mod tests {
    use super::{
        PlaybackCursorInterpolation, PlaybackIntent, PlaybackIntentOrigin, accepted_resume_intent,
        audio_output_resume_is_current, authoritative_cursor_progress, cursor_interpolation,
        normalized_playback_start, pending_resume_intent, playback_mode_for_intent,
        playback_request_number_is_current, preview_cursor_ratio, resolve_queued_playback_intent,
        should_cancel_pending_autoplay, should_queue_initialized_playback,
        should_record_last_played, should_record_playback_history,
    };
    use reson::{PlaybackRuntimeMode, PlaybackRuntimeProgress};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::super::waveform::WaveformPlayRegion;
    use wavecrate_core::{AudioFile, PlaybackLoopPolicy, PlaybackType};

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
    fn pending_playback_intent_tracks_the_latest_quantized_volume() {
        let mut intent = PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 1.0,
            path: PathBuf::from("sample.wav"),
            origin: PlaybackIntentOrigin::Explicit,
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
}
