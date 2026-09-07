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
    App, AppContext, BorrowAppContext, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent,
    PathPromptOptions, ScrollStrategy, Task, UniformListScrollHandle, Window,
};
use reson::{PlaybackRequestId, PlaybackRuntime};
use wavecrate_core::{AudioFile, AudioFileId, BrowserViewModel, SortColumn};
use wavecrate_design::{Theme, load_theme_file};
use wavecrate_media::{WaveformPreview, enumerate_audio_files_with_cancel, load_waveform_preview};

use crate::search_input::{SearchChanged, SearchInput};
use crate::theme::ActiveTheme;

mod transport;
mod view;
mod waveform;

#[derive(Default)]
struct ThemeLoadState {
    generation: u64,
    active_path: Option<PathBuf>,
    pending_path: Option<PathBuf>,
}

impl ThemeLoadState {
    fn begin(&mut self, path: PathBuf) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.pending_path = Some(path);
        self.generation
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.active_path = None;
        self.pending_path = None;
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn accept(&mut self, generation: u64, path: PathBuf) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.active_path = Some(path);
        self.pending_path = None;
        true
    }

    fn reject(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.pending_path = None;
        true
    }
}

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
    theme_status: String,
    theme_load: ThemeLoadState,
    theme_task: Option<Task<()>>,
    theme_settings_open: bool,
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
            theme_status: "Wavecrate Editorial Terminal".to_string(),
            theme_load: ThemeLoadState::default(),
            theme_task: None,
            theme_settings_open: false,
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

    fn choose_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a Wavecrate theme JSON file".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |view, cx| view.load_theme(path, cx));
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.theme_status = format!("Theme picker error: {error}");
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
    }

    fn toggle_theme_settings(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.theme_settings_open = !self.theme_settings_open;
        cx.notify();
    }

    fn toggle_playback_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_playback(cx);
    }

    fn stop_playback_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_playback();
        cx.notify();
    }

    fn reload_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.theme_load.pending_path.is_some() {
            return;
        }
        if let Some(path) = self.theme_load.active_path.clone() {
            self.load_theme(path, cx);
        } else {
            self.theme_status = "Choose a theme file before reloading".to_string();
            cx.notify();
        }
    }

    fn reset_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.theme_load.invalidate();
        self.theme_task.take();
        cx.set_global(ActiveTheme::default());
        self.theme_status = "Wavecrate Editorial Terminal".to_string();
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        cx.notify();
    }

    fn load_theme(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let generation = self.theme_load.begin(path.clone());
        self.theme_task.take();
        self.theme_status = format!("Loading {}…", path.display());
        cx.notify();
        let read_path = path.clone();
        self.theme_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_theme_file(&read_path) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.theme_load.is_current(generation) {
                    return;
                }
                view.theme_task = None;
                match result {
                    Ok(theme) => view.apply_theme(theme, path, generation, cx),
                    Err(error) => {
                        if view.theme_load.reject(generation) {
                            view.theme_status = format!("Theme rejected: {error}");
                            cx.notify();
                        }
                    }
                }
            });
        }));
    }

    fn apply_theme(
        &mut self,
        theme: Theme,
        path: PathBuf,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if !self.theme_load.accept(generation, path) {
            return;
        }
        let theme_name = theme.name.clone();
        cx.set_global(ActiveTheme(Arc::new(theme)));
        self.theme_status = theme_name;
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        cx.notify();
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

#[cfg(test)]
mod tests {
    use super::ThemeLoadState;
    use std::path::PathBuf;

    #[test]
    fn invalid_theme_keeps_the_last_good_path() {
        let good = PathBuf::from("good.json");
        let invalid = PathBuf::from("invalid.json");
        let mut state = ThemeLoadState::default();
        let good_generation = state.begin(good.clone());
        assert!(state.accept(good_generation, good.clone()));

        let invalid_generation = state.begin(invalid);
        assert!(state.reject(invalid_generation));
        assert_eq!(state.active_path, Some(good));
        assert!(state.pending_path.is_none());
    }

    #[test]
    fn late_theme_completion_after_reset_is_discarded() {
        let pending = PathBuf::from("pending.json");
        let mut state = ThemeLoadState::default();
        let generation = state.begin(pending.clone());
        state.invalidate();

        assert!(!state.accept(generation, pending));
        assert!(state.active_path.is_none());
        assert!(state.pending_path.is_none());
    }

    #[test]
    fn replacing_a_pending_theme_makes_the_older_completion_stale() {
        let first = PathBuf::from("first.json");
        let second = PathBuf::from("second.json");
        let mut state = ThemeLoadState::default();
        let first_generation = state.begin(first.clone());
        let second_generation = state.begin(second.clone());

        assert!(!state.accept(first_generation, first));
        assert!(state.accept(second_generation, second.clone()));
        assert_eq!(state.active_path, Some(second));
    }
}
