#![deny(missing_docs)]
#![deny(warnings)]

//! Browser state, scan lifecycle, selection, and keyboard navigation.
//!
//! Selection is currently a single focused row; shift-based multiselect remains future work.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    App, AppContext, BorrowAppContext, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent,
    PathPromptOptions, ScrollStrategy, Task, UniformListScrollHandle, Window,
};
use reson::{PlaybackRequestId, PlaybackRuntime};
use wavecrate_core::{
    AudioFile, AudioFileId, BrowserViewModel, FolderScope, FolderTree, FolderTreeRow, SortColumn,
};
use wavecrate_design::{Theme, load_theme_file};
use wavecrate_media::{WaveformPreview, enumerate_audio_files_with_cancel, load_waveform_preview};

use crate::search_input::{SearchChanged, SearchInput};
use crate::theme::ActiveTheme;

mod sidebar;
mod transport;
mod view;
mod waveform;

#[derive(Clone, Debug)]
struct SourceSession {
    root: PathBuf,
    label: String,
    entries: Vec<AudioFile>,
    tree: FolderTree,
    selected_folder: PathBuf,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    selected_audio_id: Option<AudioFileId>,
    last_error: Option<String>,
}

impl SourceSession {
    fn new(root: PathBuf) -> Self {
        let label = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| root.display().to_string());
        Self {
            root,
            label,
            entries: Vec::new(),
            tree: FolderTree::default(),
            selected_folder: PathBuf::from("."),
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            selected_audio_id: None,
            last_error: None,
        }
    }
}

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
    source_sessions: Vec<SourceSession>,
    active_source: Option<usize>,
    pending_source_label: Option<String>,
    pending_source_return_index: Option<usize>,
    folder_tree: Option<FolderTree>,
    folder_rows_cache: Vec<FolderTreeRow>,
    selected_folder: PathBuf,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    restore_selection_id: Option<AudioFileId>,
    restore_selection_position: Option<usize>,
    view_model: BrowserViewModel,
    selected: Option<usize>,
    waveform: Option<WaveformPreview>,
    waveform_error: Option<String>,
    waveform_render_cache: waveform::WaveformRenderCache,
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
    playback_monitor_generation: u64,
    playback_monitor_running: bool,
    playback: Option<PlaybackRuntime>,
    list_scroll_handle: UniformListScrollHandle,
    source_scroll_handle: UniformListScrollHandle,
    folder_scroll_handle: UniformListScrollHandle,
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
            source_sessions: Vec::new(),
            active_source: None,
            pending_source_label: None,
            pending_source_return_index: None,
            folder_tree: None,
            folder_rows_cache: Vec::new(),
            selected_folder: PathBuf::from("."),
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            restore_selection_id: None,
            restore_selection_position: None,
            view_model: BrowserViewModel::new(&[]),
            selected: None,
            waveform: None,
            waveform_error: None,
            waveform_render_cache: waveform::WaveformRenderCache::default(),
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
            playback_monitor_generation: 0,
            playback_monitor_running: false,
            playback: None,
            list_scroll_handle: UniformListScrollHandle::new(),
            source_scroll_handle: UniformListScrollHandle::new(),
            folder_scroll_handle: UniformListScrollHandle::new(),
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
                if let Some(index) = self.active_source
                    && let Some(session) = self.source_sessions.get_mut(index)
                {
                    session.selected_audio_id = None;
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn start_scan(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.begin_source_scan(root, None, cx);
    }

    fn begin_source_scan(
        &mut self,
        root: PathBuf,
        source_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.restore_selection_id = if source_index == self.active_source {
            self.selected_id()
        } else {
            source_index
                .and_then(|index| self.source_sessions.get(index))
                .and_then(|session| session.selected_audio_id.clone())
        };
        self.restore_selection_position = if source_index == self.active_source {
            self.selected_visible_position()
        } else {
            None
        };
        self.stop_playback();
        self.pending_source_return_index = if source_index.is_none() {
            self.active_source.or(self.pending_source_return_index)
        } else {
            None
        };
        self.active_source = source_index;
        self.pending_source_label = source_index.is_none().then(|| source_label(&root));
        self.root = Some(root.clone());
        self.entries.clear();
        self.view_model.set_entries(&self.entries);
        self.selected = None;
        self.waveform_task.take();
        self.waveform = None;
        self.waveform_error = None;
        self.folder_tree = None;
        self.folder_rows_cache.clear();
        if let Some(index) = source_index
            && let Some(session) = self.source_sessions.get(index)
        {
            self.selected_folder = session.selected_folder.clone();
            self.expanded_folders = session.expanded_folders.clone();
            self.folder_scope = session.folder_scope;
        } else {
            self.selected_folder = PathBuf::from(".");
            self.expanded_folders = BTreeSet::from([PathBuf::from(".")]);
            self.folder_scope = FolderScope::DirectOnly;
        }
        self.waveform_generation = self.waveform_generation.wrapping_add(1);
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
                    Ok(scan) => view.apply_scan_result(scan, source_index, cx),
                    Err(error) => view.apply_scan_error(source_index, error, cx),
                }
                cx.notify();
            });
        }));
    }

    fn apply_scan_result(
        &mut self,
        scan: wavecrate_media::AudioScan,
        source_index_hint: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let source_index =
            resolve_scan_source_index(&mut self.source_sessions, &scan.root, source_index_hint);

        let tree = FolderTree::from_scan(&scan.directories, &scan.files);
        let (previous_folder, previous_expanded, previous_scope, previous_selected) = {
            let session = &self.source_sessions[source_index];
            (
                session.selected_folder.clone(),
                session.expanded_folders.clone(),
                session.folder_scope,
                session.selected_audio_id.clone(),
            )
        };
        let state = tree.reconcile_state(Some(&previous_folder), &previous_expanded);
        let count = scan.files.len();
        let entries = scan.files;
        let root = scan.root;
        let skipped_entries = scan.skipped_entries;
        let session = &mut self.source_sessions[source_index];
        session.root = root.clone();
        session.label = source_label(&root);
        session.entries = entries.clone();
        session.tree = tree.clone();
        session.selected_folder = state.selected.clone();
        session.expanded_folders = state.expanded.clone();
        session.folder_scope = previous_scope;
        session.last_error = None;

        self.active_source = Some(source_index);
        self.pending_source_label = None;
        self.pending_source_return_index = None;
        self.root = Some(root);
        self.entries = entries;
        self.folder_tree = Some(tree);
        self.selected_folder = state.selected;
        self.expanded_folders = state.expanded;
        self.rebuild_folder_rows_cache();
        self.folder_scope = previous_scope;
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        self.scan_status = format!(
            "{count} audio file{} · {} skipped",
            if count == 1 { "" } else { "s" },
            skipped_entries
        );
        let restore_id = self.restore_selection_id.take().or(previous_selected);
        let restore_position = self.restore_selection_position.take();
        self.restore_projection_focus(restore_id, restore_position, cx);
    }

    fn apply_scan_error(
        &mut self,
        source_index: Option<usize>,
        error: String,
        cx: &mut Context<Self>,
    ) {
        let is_refresh = source_index.is_some();
        let fallback_index = source_index.or(self.pending_source_return_index.take());
        let Some(index) = fallback_index.filter(|index| *index < self.source_sessions.len()) else {
            self.active_source = None;
            self.pending_source_label = None;
            self.root = None;
            self.entries.clear();
            self.folder_tree = None;
            self.folder_rows_cache.clear();
            self.view_model.set_entries(&self.entries);
            self.scan_status = error;
            self.selected = None;
            self.waveform = None;
            self.waveform_error = None;
            cx.notify();
            return;
        };

        if is_refresh {
            self.source_sessions[index].last_error = Some(error.clone());
        }
        self.active_source = Some(index);
        self.pending_source_return_index = None;
        self.pending_source_label = None;
        self.root = Some(self.source_sessions[index].root.clone());
        self.entries = self.source_sessions[index].entries.clone();
        self.folder_tree = Some(self.source_sessions[index].tree.clone());
        self.selected_folder = self.source_sessions[index].selected_folder.clone();
        self.expanded_folders = self.source_sessions[index].expanded_folders.clone();
        self.rebuild_folder_rows_cache();
        self.folder_scope = self.source_sessions[index].folder_scope;
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        self.scan_status = if is_refresh {
            format!(
                "Could not refresh {}: {error}",
                self.source_sessions[index].label
            )
        } else {
            format!("Could not add source: {error}")
        };
        let restore_id = self
            .restore_selection_id
            .take()
            .or_else(|| self.source_sessions[index].selected_audio_id.clone());
        let restore_position = self.restore_selection_position.take();
        self.restore_projection_focus(restore_id, restore_position, cx);
    }

    pub(super) fn source_rows(&self) -> Vec<(usize, String, bool, Option<String>)> {
        self.source_sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                (
                    index,
                    session.label.clone(),
                    self.active_source == Some(index),
                    session.last_error.clone(),
                )
            })
            .collect()
    }

    pub(super) fn folder_rows(&self) -> &[FolderTreeRow] {
        &self.folder_rows_cache
    }

    pub(super) fn active_source_label(&self) -> Option<String> {
        self.active_source
            .and_then(|index| self.source_sessions.get(index))
            .map(|session| session.label.clone())
            .or_else(|| self.pending_source_label.clone())
    }

    pub(super) fn active_source_error(&self) -> Option<String> {
        self.active_source
            .and_then(|index| self.source_sessions.get(index))
            .and_then(|session| session.last_error.clone())
    }

    pub(super) fn active_folder_label(&self) -> String {
        if self.selected_folder == Path::new(".") {
            ".".to_string()
        } else {
            self.selected_folder.display().to_string()
        }
    }

    pub(super) fn select_source(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self
            .source_sessions
            .get(index)
            .map(|session| session.root.clone())
        else {
            return;
        };
        window.focus(&self.focus_handle);
        self.begin_source_scan(root, Some(index), cx);
    }

    pub(super) fn forget_source(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.source_sessions.len() {
            return;
        }
        let was_active = self.active_source == Some(index);
        let had_pending_scan = if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
            true
        } else {
            false
        };
        if had_pending_scan || was_active {
            self.scan_generation = self.scan_generation.wrapping_add(1);
            self.waveform_generation = self.waveform_generation.wrapping_add(1);
            self.waveform_task.take();
            self.stop_playback();
        }
        self.pending_source_return_index = None;
        self.source_sessions.remove(index);
        if self.source_sessions.is_empty() {
            self.active_source = None;
            self.pending_source_label = None;
            self.root = None;
            self.entries.clear();
            self.folder_tree = None;
            self.folder_rows_cache.clear();
            self.selected = None;
            self.waveform = None;
            self.waveform_error = None;
            self.view_model.set_entries(&self.entries);
            self.scan_status = "Add a source folder to begin".to_string();
        } else if was_active || had_pending_scan {
            let next_index = next_source_index_after_removal(
                self.active_source,
                index,
                self.source_sessions.len(),
            )
            .expect("source list is non-empty after removal");
            self.active_source = None;
            self.select_source(next_index, _window, cx);
        } else if let Some(active) = self.active_source {
            self.active_source = Some(
                next_source_index_after_removal(Some(active), index, self.source_sessions.len())
                    .expect("source list is non-empty after removal"),
            );
        }
        cx.notify();
    }

    pub(super) fn refresh_active_source(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.active_source else {
            self.scan_status = "Choose a source folder before refreshing".to_string();
            cx.notify();
            return;
        };
        let Some(root) = self
            .source_sessions
            .get(index)
            .map(|session| session.root.clone())
        else {
            return;
        };
        self.begin_source_scan(root, Some(index), cx);
    }

    pub(super) fn toggle_folder_scope(&mut self, cx: &mut Context<Self>) {
        self.folder_scope = self.folder_scope.toggled();
        self.update_active_folder_projection(cx);
    }

    pub(super) fn select_folder(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tree) = self.folder_tree.as_ref() else {
            return;
        };
        if !tree.contains(&path) {
            return;
        }
        window.focus(&self.focus_handle);
        self.selected_folder = path;
        self.update_active_folder_projection(cx);
    }

    pub(super) fn toggle_folder_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path == Path::new(".") {
            return;
        }
        if !self.expanded_folders.insert(path.clone()) {
            self.expanded_folders.remove(&path);
        }
        self.rebuild_folder_rows_cache();
        if let Some(index) = self.active_source
            && let Some(session) = self.source_sessions.get_mut(index)
        {
            session.expanded_folders = self.expanded_folders.clone();
        }
        cx.notify();
    }

    fn update_active_folder_projection(&mut self, cx: &mut Context<Self>) {
        let previous_id = self.selected_id();
        let previous_position = self.selected_visible_position();
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        if let Some(index) = self.active_source
            && let Some(session) = self.source_sessions.get_mut(index)
        {
            session.selected_folder = self.selected_folder.clone();
            session.expanded_folders = self.expanded_folders.clone();
            session.folder_scope = self.folder_scope;
            session.selected_audio_id = previous_id.clone();
        }
        self.restore_projection_focus(previous_id, previous_position, cx);
    }

    fn rebuild_folder_rows_cache(&mut self) {
        self.folder_rows_cache = self
            .folder_tree
            .as_ref()
            .map(|tree| tree.rows(&self.expanded_folders))
            .unwrap_or_default();
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
        if let Some(source_index) = self.active_source
            && let Some(session) = self.source_sessions.get_mut(source_index)
        {
            session.selected_audio_id = Some(next_id.clone());
        }
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

fn resolve_scan_source_index(
    sessions: &mut Vec<SourceSession>,
    scan_root: &Path,
    source_index_hint: Option<usize>,
) -> usize {
    if let Some(index) = sessions
        .iter()
        .position(|session| session.root == scan_root)
    {
        return index;
    }
    if let Some(index) = source_index_hint.filter(|index| {
        sessions
            .get(*index)
            .is_some_and(|session| session.root == scan_root)
    }) {
        return index;
    }
    sessions.push(SourceSession::new(scan_root.to_path_buf()));
    sessions.len() - 1
}

fn next_source_index_after_removal(
    active_source: Option<usize>,
    removed_index: usize,
    remaining_sources: usize,
) -> Option<usize> {
    if remaining_sources == 0 {
        return None;
    }
    Some(
        active_source
            .map(|active| {
                if active > removed_index {
                    active - 1
                } else {
                    active
                }
            })
            .unwrap_or(0)
            .min(remaining_sources - 1),
    )
}

fn source_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        SourceSession, ThemeLoadState, next_source_index_after_removal, resolve_scan_source_index,
    };
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

    #[test]
    fn scan_result_reuses_a_source_with_the_same_canonical_root() {
        let canonical_root = PathBuf::from("/canonical/library");
        let mut sessions = vec![SourceSession::new(canonical_root.clone())];

        let index = resolve_scan_source_index(&mut sessions, &canonical_root, None);

        assert_eq!(index, 0);
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn source_removal_remaps_active_index_and_fences_empty_state() {
        assert_eq!(next_source_index_after_removal(Some(2), 0, 2), Some(1));
        assert_eq!(next_source_index_after_removal(Some(0), 1, 2), Some(0));
        assert_eq!(next_source_index_after_removal(None, 0, 0), None);
    }
}
