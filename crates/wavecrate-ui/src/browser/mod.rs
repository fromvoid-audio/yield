#![deny(missing_docs)]
#![deny(warnings)]

//! Browser state, scan lifecycle, selection, and keyboard navigation.
//!
//! Selection is currently a single focused row; shift-based multiselect remains future work.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    App, AppContext, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent, PathPromptOptions,
    ScrollStrategy, Task, UniformListScrollHandle, Window,
};
use reson::{PlaybackRequestId, PlaybackRuntime};
use wavecrate_core::{AudioFile, AudioFileId, BrowserViewModel, SortColumn};
use wavecrate_media::{WaveformPreview, enumerate_audio_files_with_cancel, load_waveform_preview};

use crate::search_input::{SearchChanged, SearchInput};

mod transport;
mod view;

pub(crate) struct Browser {
    focus_handle: FocusHandle,
    search_input: gpui::Entity<SearchInput>,
    _search_subscription: gpui::Subscription,
    root: Option<PathBuf>,
    entries: Vec<AudioFile>,
    view_model: BrowserViewModel,
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
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(SearchInput::new);
        let search_subscription =
            cx.subscribe(&search_input, |view, input, _: &SearchChanged, cx| {
                let previous_id = view.selected_id();
                let previous_position = view.selected_visible_position();
                let query = input.read(cx).content().to_owned();
                view.view_model.set_query(&view.entries, query);
                view.restore_projection_focus(previous_id, previous_position, cx);
            });
        Self {
            focus_handle: cx.focus_handle(),
            search_input,
            _search_subscription: search_subscription,
            root: None,
            entries: Vec::new(),
            view_model: BrowserViewModel::new(&[]),
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

    fn selected_id(&self) -> Option<AudioFileId> {
        self.selected
            .and_then(|index| self.entries.get(index))
            .map(AudioFile::stable_id)
    }

    fn selected_visible_position(&self) -> Option<usize> {
        self.selected_id()
            .as_ref()
            .and_then(|id| self.view_model.visible_position_for(id))
    }

    fn restore_projection_focus(
        &mut self,
        previous_id: Option<AudioFileId>,
        previous_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let next = self
            .view_model
            .restore_focus(previous_id.as_ref(), previous_position);
        match next {
            Some(index) => self.select_entry(index, cx),
            None => {
                if self.selected.is_some() {
                    self.stop_playback();
                    self.selected = None;
                    self.waveform = None;
                    self.waveform_error = None;
                    self.waveform_generation = self.waveform_generation.wrapping_add(1);
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn start_scan(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.stop_playback();
        self.root = Some(root.clone());
        self.search_input.update(cx, |input, cx| input.reset(cx));
        self.entries.clear();
        self.view_model.set_entries(&self.entries);
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
                        view.view_model.set_entries(&view.entries);
                        view.scan_status = format!(
                            "{count} audio file{} · {} skipped",
                            if count == 1 { "" } else { "s" },
                            scan.skipped_entries
                        );
                        if count > 0
                            && let Some(row) = view.view_model.rows().first()
                        {
                            view.select_entry(row.entry_index, cx);
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
        let previous_id = self.selected_id();
        let next_id = entry.stable_id();
        if previous_id.as_ref() == Some(&next_id) {
            self.selected = Some(index);
            if let Some(position) = self
                .view_model
                .rows()
                .iter()
                .position(|row| row.entry_index == index)
            {
                self.list_scroll_handle
                    .scroll_to_item(position, ScrollStrategy::Center);
            }
            cx.notify();
            return;
        }
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
        if let Some(position) = self
            .view_model
            .rows()
            .iter()
            .position(|row| row.entry_index == index)
        {
            self.list_scroll_handle
                .scroll_to_item(position, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn choose_folder(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a sample folder".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(folder) = paths.into_iter().next() {
                    let _ = this.update(cx, |view, cx| view.start_scan(folder, cx));
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.scan_status = format!("Could not choose folder: {error}");
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
    }

    fn move_selection(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.view_model.rows().is_empty() {
            return;
        }
        let current = self
            .selected_id()
            .as_ref()
            .and_then(|id| self.view_model.visible_position_for(id))
            .unwrap_or(0) as isize;
        let next_position =
            (current + direction).clamp(0, self.view_model.rows().len() as isize - 1) as usize;
        let next = self.view_model.rows()[next_position].entry_index;
        if Some(next) != self.selected {
            self.select_entry(next, cx);
        }
    }

    fn toggle_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        let previous_id = self.selected_id();
        let previous_position = self.selected_visible_position();
        self.view_model.toggle_sort(&self.entries, column);
        self.restore_projection_focus(previous_id, previous_position, cx);
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if key.eq_ignore_ascii_case("f")
            && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
        {
            window.focus(&self.search_input.read(cx).focus_handle());
            return;
        }
        if self.search_input.read(cx).focus_handle().is_focused(window) {
            if key == "escape" {
                window.focus(&self.focus_handle);
            }
            return;
        }
        if event.is_held && key == "space" {
            return;
        }
        match key {
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

impl Focusable for Browser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
