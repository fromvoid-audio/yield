#![deny(missing_docs)]
#![deny(warnings)]

//! Audio playback lifecycle for the browser.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::Context;
use reson::{
    AudioPlayer, PlaybackRequestId, PlaybackRuntime, PlaybackRuntimeConfig, PlaybackRuntimeEvent,
    PlaybackRuntimeMode, PlaybackRuntimeProgress, PlaybackRuntimeReplacePolicy,
    PlaybackRuntimeRequest, PlaybackRuntimeSource, PlaybackRuntimeStarted,
    PlaybackRuntimeStreamPolicy,
};
use wavecrate_core::AudioFile;

use super::Browser;

const PLAYBACK_MONITOR_INTERVAL: Duration = Duration::from_millis(80);
const PLAYBACK_CURSOR_MAX_INTERPOLATION: Duration = PLAYBACK_MONITOR_INTERVAL;

/// One authoritative one-shot elapsed sample retained by the browser cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaybackCursorSample {
    pub(super) elapsed: Duration,
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
                            view.playback_cursor_sample.map(|sample| sample.elapsed),
                        );
                        view.poll_playback_events();
                        let changed = before
                            != (
                                view.playing_path.clone(),
                                view.playback_status.clone(),
                                view.playback_cursor_sample.map(|sample| sample.elapsed),
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
            || preview_cursor_ratio_for_browser(self, sample.elapsed.as_secs_f32()).is_none()
        {
            return false;
        }
        self.playback_cursor_ratio_at(now, true).is_some()
    }

    fn playback_cursor_ratio_at(&self, now: Instant, animations_enabled: bool) -> Option<f32> {
        let sample = self.playback_cursor_sample?;
        preview_cursor_ratio_for_browser(self, sample.elapsed.as_secs_f32())?;
        let elapsed_seconds = if animations_enabled {
            self.playback_cursor_interpolation
                .map(|interpolation| interpolation.value_at(now))
                .unwrap_or_else(|| sample.elapsed.as_secs_f32())
        } else {
            sample.elapsed.as_secs_f32()
        };
        preview_cursor_ratio_for_browser(self, elapsed_seconds)
    }

    fn set_playback_cursor_start(&mut self, started: &PlaybackRuntimeStarted) {
        // The browser currently queues only a one-shot span from zero. A future
        // seek/loop start needs an explicit offset-aware mapping before the
        // elapsed-time cursor can be shown safely.
        if !started.playback_start.is_finite() || started.playback_start.abs() > f32::EPSILON {
            self.clear_playback_cursor();
            return;
        }
        self.playback_cursor_sample = Some(PlaybackCursorSample {
            elapsed: Duration::ZERO,
        });
        self.playback_cursor_interpolation = None;
    }

    fn record_playback_progress(&mut self, progress: &PlaybackRuntimeProgress) {
        if self.playback_cursor_sample.is_none() {
            self.clear_playback_cursor();
            return;
        }
        let Some(elapsed) = authoritative_cursor_elapsed(progress) else {
            self.clear_playback_cursor();
            return;
        };
        let sampled_at = Instant::now();
        let interpolation = self.playback_cursor_sample.and_then(|previous| {
            let from = self
                .playback_cursor_interpolation
                .map(|interpolation| interpolation.value_at(sampled_at))
                .unwrap_or_else(|| previous.elapsed.as_secs_f32());
            let to = elapsed.as_secs_f32();
            let duration = elapsed
                .checked_sub(previous.elapsed)
                .map(|duration| duration.min(PLAYBACK_CURSOR_MAX_INTERPOLATION))
                .filter(|duration| !duration.is_zero());
            duration
                .filter(|_| (from - to).abs() > f32::EPSILON)
                .map(|duration| PlaybackCursorInterpolation {
                    from,
                    to,
                    started_at: sampled_at,
                    duration,
                })
        });
        self.playback_cursor_sample = Some(PlaybackCursorSample { elapsed });
        self.playback_cursor_interpolation = interpolation;
    }

    pub(super) fn clear_playback_cursor(&mut self) {
        self.playback_cursor_sample = None;
        self.playback_cursor_interpolation = None;
    }

    pub(super) fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.playing_path.is_some() {
            self.stop_playback();
            cx.notify();
            return;
        }
        let Some(index) = self.selected else {
            self.playback_status = "Select a sample first".to_string();
            cx.notify();
            return;
        };
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if !entry
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        {
            self.playback_status = "Audition currently supports WAV files".to_string();
            cx.notify();
            return;
        }
        if self.playback_init_task.is_some() {
            self.playback_init_entry = Some(entry);
            self.playback_init_epoch = self.playback_epoch;
            self.playback_init_generation = self.waveform_generation;
            self.playback_status = "Starting audio…".to_string();
            cx.notify();
            return;
        }
        if self.playback.is_none() {
            self.start_playback_runtime(entry, cx);
        } else {
            self.queue_playback(entry, cx);
        }
    }

    fn start_playback_runtime(&mut self, entry: AudioFile, cx: &mut Context<Self>) {
        self.playback_init_entry = Some(entry);
        self.playback_init_epoch = self.playback_epoch;
        self.playback_init_generation = self.waveform_generation;
        self.playback_status = "Starting audio…".to_string();
        self.playback_init_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let player = AudioPlayer::new()?;
                    PlaybackRuntime::spawn(player, PlaybackRuntimeConfig::default())
                        .map_err(|error| format!("Could not start audio runtime: {error}"))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.playback_init_task = None;
                let pending_entry = view.playback_init_entry.take();
                match result {
                    Ok(runtime) => {
                        view.playback = Some(runtime);
                        let should_queue = pending_entry.as_ref().is_some_and(|entry| {
                            let selected_path = view
                                .selected
                                .and_then(|index| view.entries.get(index))
                                .map(|selected| selected.path.as_path());
                            should_queue_initialized_playback(
                                view.playback_init_generation,
                                view.playback_init_epoch,
                                view.waveform_generation,
                                view.playback_epoch,
                                selected_path,
                                entry.path.as_path(),
                            )
                        });
                        if should_queue {
                            view.queue_playback(pending_entry.expect("pending playback entry"), cx);
                        } else {
                            view.playback_status = "Ready".to_string();
                        }
                    }
                    Err(error) => {
                        view.playback_status = error;
                    }
                }
                cx.notify();
            });
        }));
    }

    fn queue_playback(&mut self, entry: AudioFile, cx: &mut Context<Self>) {
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            return;
        };
        let request = PlaybackRuntimeRequest {
            source: PlaybackRuntimeSource::WavFile {
                path: entry.path.clone(),
            },
            mode: PlaybackRuntimeMode::OneShot {
                start: 0.0,
                end: 1.0,
            },
            stream_policy: PlaybackRuntimeStreamPolicy::transient_navigation(),
            volume: 1.0,
            playback_gain: 1.0,
            playback_gain_normalization: None,
            replace_policy: PlaybackRuntimeReplacePolicy::ClearPrevious,
            edit_fade: None,
            metronome: None,
        };
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
        self.pending_progress_poll = None;
        self.clear_playback_cursor();
        match handle.try_play(request) {
            Ok(request_id) => {
                self.latest_playback_request_id = Some(request_id);
                self.playing_path = Some(entry.path);
                self.playback_status = "Starting…".to_string();
                self.ensure_playback_monitor(cx);
            }
            Err(error) => {
                self.latest_playback_request_id = None;
                self.playing_path = None;
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
        self.playback_init_entry = None;
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
                    self.playback_status = "Stopped".to_string();
                }
                PlaybackRuntimeEvent::Failed { id, error }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = format!("Playback failed: {error}");
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.clear_playback_cursor();
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
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

fn preview_cursor_ratio_for_browser(browser: &Browser, elapsed_seconds: f32) -> Option<f32> {
    let preview_duration = browser.waveform.as_ref()?.duration_seconds;
    preview_cursor_ratio(preview_duration, elapsed_seconds)
}

/// Map a one-shot elapsed playback position into the bounded preview.
fn preview_cursor_ratio(preview_duration_seconds: f32, elapsed_seconds: f32) -> Option<f32> {
    if !preview_duration_seconds.is_finite()
        || preview_duration_seconds <= 0.0
        || !elapsed_seconds.is_finite()
        || elapsed_seconds < 0.0
    {
        return None;
    }
    if elapsed_seconds > preview_duration_seconds + f32::EPSILON {
        return None;
    }
    Some((elapsed_seconds / preview_duration_seconds).clamp(0.0, 1.0))
}

fn authoritative_cursor_elapsed(progress: &PlaybackRuntimeProgress) -> Option<Duration> {
    if !progress.active || progress.looping || progress.error.is_some() {
        return None;
    }
    progress.elapsed
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
        PlaybackCursorInterpolation, authoritative_cursor_elapsed,
        playback_request_number_is_current, preview_cursor_ratio,
        should_queue_initialized_playback,
    };
    use reson::PlaybackRuntimeProgress;
    use std::path::Path;
    use std::time::{Duration, Instant};

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
    fn one_shot_elapsed_maps_into_the_bounded_preview_and_hides_after_it() {
        assert_eq!(preview_cursor_ratio(2.0, 0.5), Some(0.25));
        assert_eq!(preview_cursor_ratio(6.0, 3.0), Some(0.5));
        assert_eq!(preview_cursor_ratio(6.0, 6.0), Some(1.0));
        assert_eq!(preview_cursor_ratio(6.0, 6.000_1), None);
    }

    #[test]
    fn invalid_cursor_mapping_inputs_are_hidden_instead_of_clamped() {
        assert_eq!(preview_cursor_ratio(0.0, 0.5), None);
        assert_eq!(preview_cursor_ratio(6.0, -0.5), None);
        assert_eq!(preview_cursor_ratio(6.0, f32::NAN), None);
        assert_eq!(preview_cursor_ratio(f32::NAN, 1.0), None);
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
    fn cursor_lifecycle_requires_active_error_free_one_shot_elapsed() {
        let active = PlaybackRuntimeProgress {
            active: true,
            elapsed: Some(Duration::from_millis(250)),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(
            authoritative_cursor_elapsed(&active),
            Some(Duration::from_millis(250))
        );

        let completed = PlaybackRuntimeProgress {
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_elapsed(&completed), None);

        let missing_elapsed = PlaybackRuntimeProgress {
            active: true,
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_elapsed(&missing_elapsed), None);

        let looping = PlaybackRuntimeProgress {
            active: true,
            looping: true,
            elapsed: Some(Duration::from_millis(250)),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_elapsed(&looping), None);

        let errored = PlaybackRuntimeProgress {
            active: true,
            elapsed: Some(Duration::from_millis(250)),
            error: Some("output failed".to_string()),
            ..PlaybackRuntimeProgress::default()
        };
        assert_eq!(authoritative_cursor_elapsed(&errored), None);
    }
}
