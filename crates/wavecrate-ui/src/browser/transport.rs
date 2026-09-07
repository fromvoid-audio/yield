#![deny(missing_docs)]
#![deny(warnings)]

//! Audio playback lifecycle for the browser.

use std::path::Path;
use std::time::Duration;

use gpui::Context;
use reson::{
    AudioPlayer, PlaybackRequestId, PlaybackRuntime, PlaybackRuntimeConfig, PlaybackRuntimeEvent,
    PlaybackRuntimeMode, PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest,
    PlaybackRuntimeSource, PlaybackRuntimeStreamPolicy,
};
use wavecrate_core::AudioFile;

use super::Browser;

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
                    .timer(Duration::from_millis(80))
                    .await;
                let keep_running = this
                    .update(cx, |view, cx| {
                        if view.playback_monitor_generation != generation {
                            return false;
                        }
                        let before = (view.playing_path.clone(), view.playback_status.clone());
                        view.poll_playback_events();
                        let changed =
                            before != (view.playing_path.clone(), view.playback_status.clone());
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
        if self.playing_path.is_some() && self.pending_progress_poll.is_none() {
            if let Some(runtime) = self.playback.as_ref() {
                if let Ok(request_id) = runtime.handle.try_poll_progress() {
                    self.pending_progress_poll = Some((request_id, self.playback_epoch));
                }
            }
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
                    if progress.active {
                        let percent = progress.progress.unwrap_or(0.0) * 100.0;
                        self.playback_status = format!("Playing {percent:.0}%");
                    } else {
                        self.latest_playback_request_id = None;
                        self.playing_path = None;
                        self.playback_status = "Stopped".to_string();
                    }
                }
                PlaybackRuntimeEvent::Stopped { id }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = "Stopped".to_string();
                }
                PlaybackRuntimeEvent::Failed { id, error }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = format!("Playback failed: {error}");
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.pending_progress_poll = None;
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = "Playback cancelled".to_string();
                }
                _ => {}
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
    use super::{playback_request_number_is_current, should_queue_initialized_playback};
    use std::path::Path;

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
}
