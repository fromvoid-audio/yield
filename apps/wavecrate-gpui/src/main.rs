#![deny(missing_docs)]
#![deny(warnings)]

//! Standalone GPUI sample browser for the Wavecrate reimplementation.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::{
    App, Application, Bounds, ClickEvent, Context, KeyDownEvent, ScrollStrategy, Task,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
    uniform_list,
};
use reson::{
    AudioPlayer, PlaybackRequestId, PlaybackRuntime, PlaybackRuntimeConfig, PlaybackRuntimeEvent,
    PlaybackRuntimeMode, PlaybackRuntimeReplacePolicy, PlaybackRuntimeRequest,
    PlaybackRuntimeSource, PlaybackRuntimeStreamPolicy,
};
use wavecrate_gpui::{
    AudioFile, WaveformPreview, enumerate_audio_files_with_cancel, format_duration,
    load_waveform_preview,
};

const WINDOW_WIDTH: f32 = 1_120.0;
const WINDOW_HEIGHT: f32 = 720.0;
const ROW_HEIGHT: f32 = 44.0;

struct Browser {
    root: Option<PathBuf>,
    entries: Vec<AudioFile>,
    selected: Option<usize>,
    waveform: Option<WaveformPreview>,
    waveform_error: Option<String>,
    scan_status: String,
    playback_status: String,
    scan_generation: u64,
    waveform_generation: u64,
    playback_epoch: u64,
    playing_path: Option<PathBuf>,
    latest_playback_request_id: Option<PlaybackRequestId>,
    pending_progress_poll: Option<(PlaybackRequestId, u64)>,
    scan_cancel: Option<Arc<AtomicBool>>,
    scan_task: Option<Task<()>>,
    waveform_task: Option<Task<()>>,
    playback_init_task: Option<Task<()>>,
    playback_init_entry: Option<AudioFile>,
    playback_init_epoch: u64,
    playback_init_generation: u64,
    playback_monitor_task: Option<Task<()>>,
    playback: Option<PlaybackRuntime>,
    list_scroll_handle: UniformListScrollHandle,
}

impl Browser {
    fn new() -> Self {
        Self {
            root: None,
            entries: Vec::new(),
            selected: None,
            waveform: None,
            waveform_error: None,
            scan_status: "Choose a folder or pass one on the command line".to_string(),
            playback_status: "Ready".to_string(),
            scan_generation: 0,
            waveform_generation: 0,
            playback_epoch: 0,
            playing_path: None,
            latest_playback_request_id: None,
            pending_progress_poll: None,
            scan_cancel: None,
            scan_task: None,
            waveform_task: None,
            playback_init_task: None,
            playback_init_entry: None,
            playback_init_epoch: 0,
            playback_init_generation: 0,
            playback_monitor_task: None,
            playback: None,
            list_scroll_handle: UniformListScrollHandle::new(),
        }
    }

    fn start_scan(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.stop_playback();
        self.root = Some(root.clone());
        self.entries.clear();
        self.selected = None;
        self.waveform = None;
        self.waveform_error = None;
        self.scan_generation = self.scan_generation.wrapping_add(1);
        let generation = self.scan_generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancellation));
        self.scan_status = format!("Scanning {}…", root.display());
        cx.notify();

        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { enumerate_audio_files_with_cancel(&root, Some(&cancellation)) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.scan_generation != generation {
                    return;
                }
                view.scan_cancel = None;
                match result {
                    Ok(scan) => {
                        let count = scan.files.len();
                        view.entries = scan.files;
                        view.scan_status = format!(
                            "{count} audio file{} · {} skipped",
                            if count == 1 { "" } else { "s" },
                            scan.skipped_entries
                        );
                        if count > 0 {
                            view.select_entry(0, cx);
                        }
                    }
                    Err(error) => {
                        view.scan_status = error;
                    }
                }
                cx.notify();
            });
        }));
    }

    fn select_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        self.stop_playback();
        self.selected = Some(index);
        self.waveform = None;
        self.waveform_error = None;
        self.waveform_generation = self.waveform_generation.wrapping_add(1);
        let generation = self.waveform_generation;
        let path = entry.path;
        self.playback_status = "Ready".to_string();
        self.waveform_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_waveform_preview(&path) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.waveform_generation != generation {
                    return;
                }
                match result {
                    Ok(preview) => view.waveform = Some(preview),
                    Err(error) => view.waveform_error = Some(error),
                }
                cx.notify();
            });
        }));
        self.list_scroll_handle
            .scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    fn choose_folder(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(folder) = rfd::FileDialog::new()
            .set_title("Choose a sample folder")
            .pick_folder()
        {
            self.start_scan(folder, cx);
        }
    }

    fn move_selection(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + direction).clamp(0, self.entries.len() as isize - 1) as usize;
        if Some(next) != self.selected {
            self.select_entry(next, cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.is_held {
            return;
        }
        match event.keystroke.key.as_str() {
            "space" => self.toggle_playback(cx),
            "up" | "arrowup" => self.move_selection(-1, cx),
            "down" | "arrowdown" => self.move_selection(1, cx),
            "escape" => {
                self.stop_playback();
                cx.notify();
            }
            _ => {}
        }
    }

    fn ensure_playback_monitor(&mut self, cx: &mut Context<Self>) {
        if self.playback_monitor_task.is_some() {
            return;
        }
        self.playback_monitor_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        view.poll_playback_events();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
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
                        view.ensure_playback_monitor(cx);
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

    fn stop_playback(&mut self) {
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
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
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = "Stopped".to_string();
                }
                PlaybackRuntimeEvent::Failed { id, error }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = format!("Playback failed: {error}");
                }
                PlaybackRuntimeEvent::Cancelled { id, .. }
                    if playback_request_is_current(self.latest_playback_request_id, id) =>
                {
                    self.latest_playback_request_id = None;
                    self.playing_path = None;
                    self.playback_status = "Playback cancelled".to_string();
                }
                _ => {}
            }
        }
    }

    fn render_waveform(&self) -> impl IntoElement {
        let mut bars = div()
            .flex()
            .items_center()
            .gap(px(1.0))
            .h(px(112.0))
            .px_2()
            .bg(rgb(0x0d1118));
        if let Some(preview) = self.waveform.as_ref() {
            for peak in &preview.peaks {
                bars = bars.child(
                    div()
                        .flex_none()
                        .w(px(2.0))
                        .h(px((peak * 92.0).max(3.0)))
                        .bg(rgb(0x67d9a2)),
                );
            }
        }
        bars
    }

    fn render_details(&self) -> impl IntoElement {
        let mut details = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(350.0))
            .p_4()
            .bg(rgb(0x171d28));
        if let Some(index) = self.selected {
            if let Some(entry) = self.entries.get(index) {
                details = details
                    .child(
                        div()
                            .text_lg()
                            .child(entry.relative_path.display().to_string()),
                    )
                    .child(div().text_sm().text_color(rgb(0x9ba7b8)).child(format!(
                                "{} · {} bytes · {} Hz",
                                format_duration(entry.duration_seconds),
                                entry.byte_len,
                                entry.sample_rate
                                    .map(|rate| rate.to_string())
                                    .unwrap_or_else(|| "unknown".to_string())
                    )))
                    .child(self.render_waveform());
                if let Some(preview) = self.waveform.as_ref() {
                    details =
                        details.child(div().text_sm().text_color(rgb(0x9ba7b8)).child(format!(
                            "Waveform preview: {}",
                            format_duration(Some(preview.duration_seconds))
                        )));
                }
                if let Some(error) = self.waveform_error.as_ref() {
                    details = details.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf0a6a6))
                            .child(error.clone()),
                    );
                } else if self.waveform.is_none() {
                    details = details.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9ba7b8))
                            .child("Loading bounded waveform preview…"),
                    );
                }
                details = details.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x9ba7b8))
                        .child("Space: play or stop · Up/Down: select · Esc: stop"),
                );
            }
        } else {
            details = details.justify_center().items_center().child(
                div()
                    .text_sm()
                    .child("Select a sample to inspect its waveform"),
            );
        }
        details
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        if let Some(runtime) = self.playback.as_ref() {
            let _ = runtime.handle.try_shutdown();
        }
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root_label = self
            .root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No folder selected".to_string());
        let list = if self.entries.is_empty() {
            div()
                .flex()
                .flex_1()
                .justify_center()
                .items_center()
                .text_color(rgb(0x9ba7b8))
                .child(self.scan_status.clone())
        } else {
            div().flex_1().child(
                uniform_list(
                    "sample-list",
                    self.entries.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                let entry = view.entries[index].clone();
                                let selected = view.selected == Some(index);
                                let background = if selected {
                                    rgb(0x294d4b)
                                } else {
                                    rgb(0x171d28)
                                };
                                div()
                                    .id(index)
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap_2()
                                    .h(px(ROW_HEIGHT))
                                    .px_3()
                                    .cursor_pointer()
                                    .bg(background)
                                    .border_1()
                                    .border_color(rgb(0x242e3d))
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.select_entry(index, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(entry.relative_path.display().to_string()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_sm()
                                            .text_color(rgb(0x9ba7b8))
                                            .child(format_duration(entry.duration_seconds)),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.list_scroll_handle.clone()),
            )
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x10141c))
            .text_color(rgb(0xe6edf5))
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .bg(rgb(0x171d28))
                    .border_1()
                    .border_color(rgb(0x242e3d))
                    .child(div().text_xl().child("Wavecrate GPUI"))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(0x9ba7b8))
                            .child(root_label),
                    )
                    .child(
                        div()
                            .id("choose-folder")
                            .cursor_pointer()
                            .rounded_sm()
                            .px_3()
                            .py_2()
                            .bg(rgb(0x2c8f79))
                            .on_click(cx.listener(Self::choose_folder))
                            .child("Choose folder"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_3()
                    .p_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap_2()
                            .bg(rgb(0x171d28))
                            .border_1()
                            .border_color(rgb(0x242e3d))
                            .child(
                                div()
                                    .flex_none()
                                    .px_3()
                                    .py_2()
                                    .text_sm()
                                    .text_color(rgb(0x9ba7b8))
                                    .child(self.scan_status.clone()),
                            )
                            .child(list),
                    )
                    .child(self.render_details()),
            )
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(rgb(0x9ba7b8))
                    .child(self.playback_status.clone()),
            )
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

fn main() {
    let initial_root = env::args_os().nth(1).map(PathBuf::from);
    Application::new().run(move |cx: &mut App| {
        let initial_root = initial_root.clone();
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                let view = cx.new(|_| Browser::new());
                if let Some(root) = initial_root {
                    view.update(cx, |view, cx| view.start_scan(root, cx));
                }
                view
            },
        )
        .expect("open GPUI window");
        cx.activate(true);
    });
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
