//! Native accessibility controls owned by the browser window.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use async_channel::Receiver;
use gpui::{Context, FocusHandle, Task, Window, canvas, prelude::*};
use raw_window_handle::HasWindowHandle;
use yield_core::{
    COLLECTION_COUNT, PLAYBACK_TYPE_FILTERS, RATING_FILTER_LEVELS, SortColumn, SortDirection,
};
use yield_platform::accessibility::{
    AxAction, AxChild, AxEvent, AxFrame, AxRegistry, AxRole, AxSlider,
};

use super::{Browser, tag_library};

pub(super) const ADD_SOURCE: &str = "browser-add-source";
pub(super) const FOLDER_SCOPE: &str = "browser-folder-scope";
pub(super) const REFRESH_SOURCE: &str = "browser-refresh-source";
pub(super) const SEARCH: &str = "browser-search";
pub(super) const TAG_FILTER_INPUT: &str = "browser-tag-filter-input";
pub(super) const TAG_FILTER_TOGGLE: &str = "browser-tag-filter-toggle";
pub(super) const RANDOM_NAVIGATION: &str = "browser-random-navigation";
pub(super) const NAME_VIEW_MODE: &str = "browser-name-view-mode";
const PLAY: &str = "transport-play";
const STOP: &str = "transport-stop";
const LOOP: &str = "transport-loop";
const SIMILAR: &str = "transport-similar-sections";
const BPM_SNAP: &str = "transport-bpm-snap";
const ZERO_CROSSING: &str = "transport-zero-crossing";
const BEAT_GUIDES: &str = "transport-beat-guides";
const BEAT_COUNT: &str = "transport-beat-count";
const METRONOME: &str = "transport-metronome";
const RANDOM_AUDITION: &str = "transport-random-audition";
const NORMALIZED_AUDITION: &str = "transport-normalized-audition";
const VOLUME: &str = "transport-volume";
const WAVEFORM: &str = "transport-waveform";
const PLAYMARK_TIMING: &str = "transport-playmark-timing";

fn button(id: &'static str, label: &'static str, value: Option<String>, enabled: bool) -> AxChild {
    AxChild {
        id: id.into(),
        role: AxRole::Button,
        label: label.into(),
        value,
        enabled,
    }
}

fn toggle(id: &'static str, label: &'static str, on: bool, enabled: bool) -> AxChild {
    AxChild {
        id: id.into(),
        role: AxRole::CheckBox,
        label: label.into(),
        value: Some(if on { "On" } else { "Off" }.into()),
        enabled,
    }
}

pub(super) fn sort_id(column: SortColumn) -> &'static str {
    match column {
        SortColumn::Name => "browser-sort-name",
        SortColumn::Rating => "browser-sort-rating",
        SortColumn::PlaybackType => "browser-sort-type",
        SortColumn::Collection => "browser-sort-collection",
        SortColumn::Extension => "browser-sort-extension",
        SortColumn::Size => "browser-sort-size",
        SortColumn::LastPlayed => "browser-sort-last-played",
    }
}

pub(super) fn collection_filter_id(slot: usize) -> String {
    format!("browser-collection-filter-{slot}")
}

pub(super) fn playback_type_filter_id(index: usize) -> String {
    format!("browser-type-filter-{index}")
}

pub(super) fn rating_filter_id(index: usize) -> String {
    format!("browser-rating-filter-{index}")
}

fn visible_sort_columns(window: &Window, cx: &Context<Browser>) -> Vec<(SortColumn, &'static str)> {
    let theme = crate::theme::active(cx);
    let visibility =
        super::view::browser_column_visibility(f32::from(window.bounds().size.width), &theme);
    let mut columns = vec![(SortColumn::Name, "Name"), (SortColumn::Rating, "Rating")];
    if visibility.playback_type {
        columns.push((SortColumn::PlaybackType, "Type"));
    }
    columns.extend([
        (SortColumn::Collection, "Collection"),
        (SortColumn::Extension, "Extension"),
        (SortColumn::Size, "Size"),
    ]);
    if visibility.last_played {
        columns.push((SortColumn::LastPlayed, "Last Played"));
    }
    columns
}

pub(super) fn source_id(root: &Path) -> String {
    format!("browser-source:{:?}", root.as_os_str())
}

pub(super) fn forget_id(root: &Path) -> String {
    format!("browser-forget-source:{:?}", root.as_os_str())
}

pub(super) fn folder_id(root: &Path, folder: &Path) -> String {
    format!(
        "browser-folder:{:?}:{:?}",
        root.as_os_str(),
        folder.as_os_str()
    )
}

pub(super) fn folder_toggle_id(root: &Path, folder: &Path) -> String {
    format!(
        "browser-folder-toggle:{:?}:{:?}",
        root.as_os_str(),
        folder.as_os_str()
    )
}

pub(super) fn sample_id(path: &Path) -> String {
    format!("browser-sample:{:?}", path.as_os_str())
}

pub(super) struct BrowserAccessibility {
    registry: Rc<RefCell<AxRegistry>>,
    last_controls: RefCell<Vec<AxChild>>,
    last_volume: Cell<Option<f32>>,
    visible_sources: Vec<PathBuf>,
    visible_folders: Vec<PathBuf>,
    visible_samples: Vec<PathBuf>,
    control_focus: HashMap<String, FocusHandle>,
    add_focus: FocusHandle,
    scope_focus: FocusHandle,
    refresh_focus: FocusHandle,
    _events: Task<()>,
    _activation: gpui::Subscription,
}

impl BrowserAccessibility {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Browser>) -> Option<Self> {
        let handle = match window.window_handle() {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("browser accessibility window handle: {error}");
                return None;
            }
        };
        let (registry, receiver) = match AxRegistry::new(handle) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("browser accessibility registry: {error:?}");
                return None;
            }
        };
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            view.sync_browser_accessibility(window, cx);
            window.refresh();
            cx.notify();
        });
        let mut control_focus = HashMap::new();
        for id in [
            RANDOM_NAVIGATION,
            NAME_VIEW_MODE,
            TAG_FILTER_TOGGLE,
            PLAY,
            STOP,
            LOOP,
            SIMILAR,
            BPM_SNAP,
            ZERO_CROSSING,
            BEAT_GUIDES,
            METRONOME,
            RANDOM_AUDITION,
            NORMALIZED_AUDITION,
        ] {
            control_focus.insert(id.to_owned(), cx.focus_handle());
        }
        for column in [
            SortColumn::Name,
            SortColumn::Rating,
            SortColumn::PlaybackType,
            SortColumn::Collection,
            SortColumn::Extension,
            SortColumn::Size,
            SortColumn::LastPlayed,
        ] {
            control_focus.insert(sort_id(column).to_owned(), cx.focus_handle());
        }
        for slot in 0..COLLECTION_COUNT {
            control_focus.insert(collection_filter_id(slot), cx.focus_handle());
        }
        for index in 0..PLAYBACK_TYPE_FILTERS.len() {
            control_focus.insert(playback_type_filter_id(index), cx.focus_handle());
        }
        for index in 0..RATING_FILTER_LEVELS.len() {
            control_focus.insert(rating_filter_id(index), cx.focus_handle());
        }
        Some(Self {
            registry: Rc::new(RefCell::new(registry)),
            last_controls: RefCell::new(Vec::new()),
            last_volume: Cell::new(None),
            visible_sources: Vec::new(),
            visible_folders: Vec::new(),
            visible_samples: Vec::new(),
            control_focus,
            add_focus: cx.focus_handle(),
            scope_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            _events: Self::receive(receiver, window, cx),
            _activation: activation,
        })
    }

    fn receive(
        receiver: Receiver<AxEvent>,
        window: &Window,
        cx: &mut Context<Browser>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update_in(cx, |view, window, cx| {
                        let Some(ax) = view.accessibility.as_ref() else {
                            return;
                        };
                        if tag_library::is_open(view) || !ax.registry.borrow().contains(&event.id) {
                            return;
                        }
                        let focus = if event.id == SEARCH {
                            view.search_input.read(cx).focus_handle()
                        } else if event.id == TAG_FILTER_INPUT {
                            view.tag_filter_input.read(cx).focus_handle()
                        } else if event.id == BEAT_COUNT {
                            view.beat_count_input.read(cx).focus_handle()
                        } else if event.id == PLAYMARK_TIMING {
                            view.playmark_timing_input.read(cx).focus_handle()
                        } else if event.id == VOLUME {
                            view.volume_focus_handle.clone()
                        } else if event.id == WAVEFORM {
                            view.waveform_focus_handle.clone()
                        } else if let Some(focus) = ax.focus_handle(&event.id) {
                            focus
                        } else {
                            return;
                        };
                        let source_action =
                            view.source_rows()
                                .into_iter()
                                .find_map(|(index, root, _, _, _)| {
                                    ax.visible_sources
                                        .contains(&root)
                                        .then(|| {
                                            if event.id == source_id(&root) {
                                                Some((index, false))
                                            } else if event.id == forget_id(&root) {
                                                Some((index, true))
                                            } else {
                                                None
                                            }
                                        })
                                        .flatten()
                                });
                        let folder_action = view.root.as_deref().and_then(|root| {
                            view.folder_rows().into_iter().find_map(|row| {
                                if !ax.visible_folders.contains(&row.path) {
                                    return None;
                                }
                                if event.id == folder_id(root, &row.path) {
                                    Some((row.path.clone(), false))
                                } else if row.has_children
                                    && row.path != Path::new(".")
                                    && event.id == folder_toggle_id(root, &row.path)
                                {
                                    Some((row.path.clone(), true))
                                } else {
                                    None
                                }
                            })
                        });
                        let sample_action = view.view_model.rows().iter().find_map(|row| {
                            let entry = view.entries.get(row.entry_index)?;
                            (ax.visible_samples.contains(&entry.path)
                                && event.id == sample_id(&entry.path))
                            .then_some(row.entry_index)
                        });
                        let sort_action =
                            visible_sort_columns(window, cx)
                                .into_iter()
                                .find_map(|(column, _)| {
                                    (event.id == sort_id(column)).then_some(column)
                                });
                        let collection_action = (0..COLLECTION_COUNT)
                            .find(|&slot| event.id == collection_filter_id(slot));
                        let playback_type_action = PLAYBACK_TYPE_FILTERS
                            .iter()
                            .enumerate()
                            .find_map(|(index, &kind)| {
                                (event.id == playback_type_filter_id(index)).then_some(kind)
                            });
                        let rating_action =
                            RATING_FILTER_LEVELS
                                .iter()
                                .enumerate()
                                .find_map(|(index, &level)| {
                                    (event.id == rating_filter_id(index)).then_some(level)
                                });
                        let extra_action = matches!(
                            event.id.as_str(),
                            TAG_FILTER_INPUT
                                | TAG_FILTER_TOGGLE
                                | RANDOM_NAVIGATION
                                | NAME_VIEW_MODE
                        ) || sort_action.is_some()
                            || collection_action.is_some()
                            || playback_type_action.is_some()
                            || rating_action.is_some();
                        let transport_action = matches!(
                            event.id.as_str(),
                            PLAY | STOP
                                | LOOP
                                | SIMILAR
                                | BPM_SNAP
                                | ZERO_CROSSING
                                | BEAT_GUIDES
                                | BEAT_COUNT
                                | METRONOME
                                | RANDOM_AUDITION
                                | NORMALIZED_AUDITION
                                | VOLUME
                                | WAVEFORM
                                | PLAYMARK_TIMING
                        );
                        if !matches!(
                            event.id.as_str(),
                            ADD_SOURCE | FOLDER_SCOPE | REFRESH_SOURCE | SEARCH
                        ) && source_action.is_none()
                            && folder_action.is_none()
                            && sample_action.is_none()
                            && !extra_action
                            && !transport_action
                        {
                            return;
                        }
                        if view.active_source.is_none()
                            && (collection_action.is_some()
                                || playback_type_action.is_some()
                                || rating_action.is_some())
                        {
                            return;
                        }
                        if sample_action.is_some() && super::tag_filter::scope_pending(view) {
                            return;
                        }
                        if matches!(
                            event.id.as_str(),
                            LOOP | ZERO_CROSSING | METRONOME | NORMALIZED_AUDITION | VOLUME
                        ) && !view.preferences_editable()
                        {
                            return;
                        }
                        if event.id == RANDOM_AUDITION && !view.random_audition_available() {
                            return;
                        }
                        if event.id == PLAY
                            && view.selected.is_none()
                            && view.playing_path.is_none()
                        {
                            return;
                        }
                        if event.id == WAVEFORM && view.waveform.is_none() {
                            return;
                        }
                        if matches!(event.id.as_str(), BPM_SNAP | BEAT_GUIDES | BEAT_COUNT)
                            && view.waveform.is_none()
                        {
                            return;
                        }
                        if event.id == SIMILAR
                            && (view.waveform.is_none() || view.waveform_play_region.is_none())
                        {
                            return;
                        }
                        if event.id == STOP
                            && view.playing_path.is_none()
                            && view.playback_init_intent.is_none()
                        {
                            return;
                        }
                        if event.id == PLAYMARK_TIMING
                            && !view.playmark_timing_editor.as_ref().is_some_and(|editor| {
                                view.playmark_timing_editor_is_current(editor)
                            })
                        {
                            return;
                        }
                        cx.activate(true);
                        window.activate_window();
                        window.focus(&focus);
                        if event.id == SEARCH
                            || event.id == TAG_FILTER_INPUT
                            || event.id == PLAYMARK_TIMING
                        {
                            match event.action {
                                AxAction::SetValue(value) => {
                                    let search_input = if event.id == SEARCH {
                                        view.search_input.clone()
                                    } else if event.id == PLAYMARK_TIMING {
                                        view.playmark_timing_input.clone()
                                    } else {
                                        view.tag_filter_input.clone()
                                    };
                                    window.defer(cx, move |_, cx| {
                                        search_input
                                            .update(cx, |input, cx| input.set_content(value, cx));
                                    });
                                }
                                AxAction::Focus => {}
                                AxAction::Press | AxAction::Increment | AxAction::Decrement => {
                                    return;
                                }
                            }
                        } else if event.id == BEAT_COUNT {
                            match event.action {
                                AxAction::SetValue(value) => {
                                    let input = view.beat_count_input.clone();
                                    window.defer(cx, move |_, cx| {
                                        input.update(cx, |input, cx| input.set_content(value, cx));
                                    });
                                }
                                AxAction::Focus => {}
                                _ => return,
                            }
                        } else if event.id == VOLUME {
                            match event.action {
                                AxAction::SetValue(value) => {
                                    let Ok(value) = value.parse::<f32>() else {
                                        return;
                                    };
                                    if !view.set_volume_from_accessibility(value, cx) {
                                        return;
                                    }
                                }
                                AxAction::Increment => {
                                    view.change_volume_from_key("arrowright", false, cx);
                                }
                                AxAction::Decrement => {
                                    view.change_volume_from_key("arrowleft", false, cx);
                                }
                                AxAction::Focus => {}
                                AxAction::Press => return,
                            }
                        } else if event.id == WAVEFORM {
                            if event.action != AxAction::Focus {
                                return;
                            }
                        } else if event.action == AxAction::Press {
                            match event.id.as_str() {
                                PLAY => view.toggle_playback(cx),
                                STOP => {
                                    view.stop_playback();
                                    cx.notify();
                                }
                                LOOP => view.toggle_loop(cx),
                                SIMILAR => super::similar_sections::toggle(view, cx),
                                BPM_SNAP => {
                                    view.bpm_snap_enabled = !view.bpm_snap_enabled;
                                    cx.notify();
                                }
                                ZERO_CROSSING => super::zero_crossing::toggle_from_accessibility(
                                    view, window, cx,
                                ),
                                BEAT_GUIDES => {
                                    view.beat_guides_enabled = !view.beat_guides_enabled;
                                    cx.notify();
                                }
                                METRONOME => view.toggle_metronome(cx),
                                RANDOM_AUDITION => view.request_random_audition(false, cx),
                                NORMALIZED_AUDITION => view.toggle_normalized_audition(cx),
                                ADD_SOURCE => view.prompt_for_source(cx),
                                FOLDER_SCOPE => view.toggle_folder_scope(cx),
                                REFRESH_SOURCE => view.refresh_active_source(cx),
                                TAG_FILTER_TOGGLE => super::tag_filter::toggle(view, cx),
                                RANDOM_NAVIGATION => view.toggle_random_navigation(cx),
                                NAME_VIEW_MODE => view.toggle_sample_name_view_mode(cx),
                                _ => match source_action {
                                    Some((index, true)) => {
                                        view.forget_source(index, window, cx);
                                        if let Some(add_focus) = view.ax_focus(ADD_SOURCE) {
                                            window.focus(&add_focus);
                                        }
                                    }
                                    Some((index, false)) => {
                                        view.select_source(index, window, cx);
                                        window.focus(&focus);
                                    }
                                    None => match folder_action {
                                        Some((path, true)) => view.toggle_folder_expanded(path, cx),
                                        Some((path, false)) => {
                                            view.select_folder(path, window, cx);
                                            window.focus(&focus);
                                        }
                                        None => match sample_action {
                                            Some(index) => {
                                                view.select_entry_from_accessibility(index, cx)
                                            }
                                            _ => {
                                                if let Some(column) = sort_action {
                                                    view.toggle_sort(column, cx);
                                                } else if let Some(slot) = collection_action {
                                                    view.toggle_collection_filter(slot, cx);
                                                } else if let Some(kind) = playback_type_action {
                                                    view.toggle_playback_type_filter(kind, cx);
                                                } else if let Some(level) = rating_action {
                                                    view.toggle_rating_filter(level, cx);
                                                } else {
                                                    return;
                                                }
                                            }
                                        },
                                    },
                                },
                            }
                        }
                        view.sync_browser_accessibility(window, cx);
                        window.refresh();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn focus_handle(&self, id: &str) -> Option<FocusHandle> {
        match id {
            ADD_SOURCE => Some(self.add_focus.clone()),
            FOLDER_SCOPE => Some(self.scope_focus.clone()),
            REFRESH_SOURCE => Some(self.refresh_focus.clone()),
            _ => self.control_focus.get(id).cloned(),
        }
    }
}

impl Browser {
    pub(super) fn ax_source_ids(&self, root: &Path) -> (Option<String>, Option<String>) {
        if self.accessibility.is_some() {
            (Some(source_id(root)), Some(forget_id(root)))
        } else {
            (None, None)
        }
    }

    pub(super) fn ax_folder_ids(&self, folder: &Path) -> (Option<String>, Option<String>) {
        match (self.accessibility.as_ref(), self.root.as_deref()) {
            (Some(_), Some(root)) => (
                Some(folder_id(root, folder)),
                Some(folder_toggle_id(root, folder)),
            ),
            _ => (None, None),
        }
    }

    pub(super) fn ax_sample_id(&self, path: &Path) -> Option<String> {
        self.accessibility.as_ref().map(|_| sample_id(path))
    }

    pub(super) fn update_ax_source_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.source_rows();
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let visible_sources = range
            .filter_map(|position| rows.get(position).map(|row| row.1.clone()))
            .collect::<Vec<_>>();
        let mut visible_ids = HashSet::new();
        for root in &visible_sources {
            for id in [source_id(root), forget_id(root)] {
                visible_ids.insert(id.clone());
                ax.control_focus
                    .entry(id)
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        ax.control_focus.retain(|id, _| {
            (!id.starts_with("browser-source:") && !id.starts_with("browser-forget-source:"))
                || visible_ids.contains(id)
        });
        ax.visible_sources = visible_sources;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn update_ax_folder_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.folder_rows().to_vec();
        let Some(root) = self.root.as_deref() else {
            return;
        };
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let visible_folders = range
            .filter_map(|position| rows.get(position).map(|row| row.path.clone()))
            .collect::<Vec<_>>();
        let mut visible_ids = HashSet::new();
        for folder in &visible_folders {
            let has_toggle = rows
                .iter()
                .any(|row| row.path == *folder && row.has_children && row.path != Path::new("."));
            let id = folder_id(root, folder);
            visible_ids.insert(id.clone());
            ax.control_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle());
            if has_toggle {
                let id = folder_toggle_id(root, folder);
                visible_ids.insert(id.clone());
                ax.control_focus
                    .entry(id)
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        ax.control_focus.retain(|id, _| {
            (!id.starts_with("browser-folder:") && !id.starts_with("browser-folder-toggle:"))
                || visible_ids.contains(id)
        });
        ax.visible_folders = visible_folders;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn update_ax_sample_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let visible_samples = range
            .filter_map(|position| {
                let row = self.view_model.rows().get(position)?;
                self.entries
                    .get(row.entry_index)
                    .map(|entry| entry.path.clone())
            })
            .collect::<Vec<_>>();
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let mut visible_ids = HashSet::new();
        for path in &visible_samples {
            let id = sample_id(path);
            visible_ids.insert(id.clone());
            ax.control_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle());
        }
        ax.control_focus
            .retain(|id, _| !id.starts_with("browser-sample:") || visible_ids.contains(id));
        ax.visible_samples = visible_samples;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn sync_browser_accessibility(&self, window: &Window, cx: &mut Context<Self>) {
        let Some(ax) = self.accessibility.as_ref() else {
            return;
        };
        let controls = if tag_library::is_open(self) {
            Vec::new()
        } else {
            let mut controls = vec![
                AxChild {
                    id: SEARCH.into(),
                    role: AxRole::TextField,
                    label: "Search samples".into(),
                    value: Some(self.search_input.read(cx).content().to_owned()),
                    enabled: true,
                },
                AxChild {
                    id: ADD_SOURCE.into(),
                    role: AxRole::Button,
                    label: "Add source folder".into(),
                    value: None,
                    enabled: true,
                },
            ];
            let has_waveform = self.waveform.is_some();
            let can_play = self.selected.is_some() || self.playing_path.is_some();
            controls.extend([
                button(
                    PLAY,
                    "Play or pause sample",
                    Some(
                        if self.playing_path.is_some() {
                            "Playing"
                        } else {
                            "Stopped"
                        }
                        .into(),
                    ),
                    can_play,
                ),
                button(
                    STOP,
                    "Stop playback",
                    None,
                    self.playing_path.is_some() || self.playback_init_intent.is_some(),
                ),
                toggle(
                    LOOP,
                    "Loop selection",
                    self.loop_toolbar_enabled(),
                    self.preferences_editable(),
                ),
                toggle(
                    SIMILAR,
                    "Find similar sections",
                    self.similar_sections.enabled(),
                    has_waveform && self.waveform_play_region.is_some(),
                ),
                toggle(
                    BPM_SNAP,
                    "Snap selection to beats",
                    self.bpm_snap_enabled,
                    has_waveform,
                ),
                toggle(
                    ZERO_CROSSING,
                    "Snap to zero crossing",
                    self.zero_crossing.enabled(),
                    self.preferences_editable(),
                ),
                toggle(
                    BEAT_GUIDES,
                    "Beat guides",
                    self.beat_guides_enabled,
                    has_waveform,
                ),
                toggle(
                    METRONOME,
                    "Metronome",
                    self.metronome_toolbar_enabled(),
                    self.preferences_editable(),
                ),
                button(
                    RANDOM_AUDITION,
                    "Audition random sample",
                    None,
                    self.random_audition_available(),
                ),
                toggle(
                    NORMALIZED_AUDITION,
                    "Normalize audition volume",
                    self.normalized_audition.enabled(),
                    self.preferences_editable(),
                ),
                AxChild {
                    id: VOLUME.into(),
                    role: AxRole::Slider,
                    label: "Audition volume".into(),
                    value: None,
                    enabled: self.preferences_editable(),
                },
                AxChild {
                    id: WAVEFORM.into(),
                    role: AxRole::Group,
                    label: "Sample waveform and selection".into(),
                    value: self.selected_audio_path().map(|path| {
                        let sample = path.file_name().unwrap_or_default().to_string_lossy();
                        match self.waveform_play_region {
                            Some(region) => format!(
                                "{sample}, selection {:.1}% to {:.1}%",
                                region.start * 100.0,
                                region.end * 100.0
                            ),
                            None => format!("{sample}, no selection"),
                        }
                    }),
                    enabled: has_waveform,
                },
            ]);
            if self
                .playmark_timing_editor
                .as_ref()
                .is_some_and(|editor| self.playmark_timing_editor_is_current(editor))
            {
                controls.push(AxChild {
                    id: PLAYMARK_TIMING.into(),
                    role: AxRole::TextField,
                    label: "Selection timing".into(),
                    value: Some(self.playmark_timing_input.read(cx).content().to_owned()),
                    enabled: true,
                });
            }
            controls.push(AxChild {
                id: BEAT_COUNT.into(),
                role: AxRole::TextField,
                label: "Beats per selection".into(),
                value: Some(self.beat_count_input.read(cx).content(cx)),
                enabled: has_waveform,
            });
            let source_rows = self.source_rows();
            for root in &ax.visible_sources {
                let Some((_, _, label, selected, error)) =
                    source_rows.iter().find(|row| &row.1 == root)
                else {
                    continue;
                };
                let value = match (*selected, error.as_deref()) {
                    (true, Some(error)) => format!("Selected, error: {error}"),
                    (false, Some(error)) => format!("Error: {error}"),
                    (true, None) => "Selected".into(),
                    (false, None) => "Not selected".into(),
                };
                controls.push(AxChild {
                    id: source_id(root),
                    role: AxRole::Row,
                    label: label.clone(),
                    value: Some(value),
                    enabled: true,
                });
                controls.push(AxChild {
                    id: forget_id(root),
                    role: AxRole::Button,
                    label: format!("Forget source {label}"),
                    value: None,
                    enabled: true,
                });
            }
            if let Some(root) = self.root.as_deref() {
                let folder_rows = self.folder_rows();
                for path in &ax.visible_folders {
                    let Some(row) = folder_rows.iter().find(|row| &row.path == path) else {
                        continue;
                    };
                    let label = if row.path == Path::new(".") {
                        "All folders".to_owned()
                    } else {
                        row.path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| row.path.display().to_string())
                    };
                    controls.push(AxChild {
                        id: folder_id(root, path),
                        role: AxRole::Row,
                        label: label.clone(),
                        value: Some(format!(
                            "{}, {} audio {}",
                            if self.selected_folder == row.path {
                                "Selected"
                            } else {
                                "Not selected"
                            },
                            row.audio_file_count,
                            if row.audio_file_count == 1 {
                                "file"
                            } else {
                                "files"
                            }
                        )),
                        enabled: true,
                    });
                    if row.has_children && row.path != Path::new(".") {
                        controls.push(AxChild {
                            id: folder_toggle_id(root, path),
                            role: AxRole::Button,
                            label: format!(
                                "{} folder {label}",
                                if row.expanded { "Collapse" } else { "Expand" }
                            ),
                            value: Some(
                                if row.expanded {
                                    "Expanded"
                                } else {
                                    "Collapsed"
                                }
                                .into(),
                            ),
                            enabled: true,
                        });
                    }
                }
            }
            controls.push(AxChild {
                id: FOLDER_SCOPE.into(),
                role: AxRole::Button,
                label: "Folder scope".into(),
                value: Some(self.folder_scope.label().into()),
                enabled: true,
            });
            controls.push(AxChild {
                id: REFRESH_SOURCE.into(),
                role: AxRole::Button,
                label: "Refresh source".into(),
                value: None,
                enabled: true,
            });
            let active_source = self.active_source.is_some();
            for slot in 0..COLLECTION_COUNT {
                controls.push(AxChild {
                    id: collection_filter_id(slot),
                    role: AxRole::CheckBox,
                    label: format!("Collection {} filter", slot + 1),
                    value: Some(
                        if self.view_model.collection_filter() == Some(slot) {
                            "On"
                        } else {
                            "Off"
                        }
                        .into(),
                    ),
                    enabled: active_source,
                });
            }
            for (index, kind) in PLAYBACK_TYPE_FILTERS.iter().enumerate() {
                controls.push(AxChild {
                    id: playback_type_filter_id(index),
                    role: AxRole::CheckBox,
                    label: format!("{} filter", if index == 0 { "1-Shot" } else { "Loop" }),
                    value: Some(
                        if self.view_model.playback_type_filter().contains_type(*kind) {
                            "On"
                        } else {
                            "Off"
                        }
                        .into(),
                    ),
                    enabled: active_source,
                });
            }
            for (index, level) in RATING_FILTER_LEVELS.iter().enumerate() {
                controls.push(AxChild {
                    id: rating_filter_id(index),
                    role: AxRole::CheckBox,
                    label: format!("Rating {level} filter"),
                    value: Some(
                        if self.view_model.rating_filter().contains_level(*level) {
                            "On"
                        } else {
                            "Off"
                        }
                        .into(),
                    ),
                    enabled: active_source,
                });
            }
            controls.push(AxChild {
                id: TAG_FILTER_TOGGLE.into(),
                role: AxRole::CheckBox,
                label: "Tag filter".into(),
                value: Some(
                    if self.tag_filter.desired_enabled {
                        "On"
                    } else {
                        "Off"
                    }
                    .into(),
                ),
                enabled: true,
            });
            controls.push(AxChild {
                id: TAG_FILTER_INPUT.into(),
                role: AxRole::TextField,
                label: "Filter tags".into(),
                value: Some(self.tag_filter_input.read(cx).content().to_owned()),
                enabled: true,
            });
            for (column, label) in visible_sort_columns(window, cx) {
                let sort = self.view_model.sort();
                controls.push(AxChild {
                    id: sort_id(column).into(),
                    role: AxRole::Button,
                    label: format!("Sort by {label}"),
                    value: Some(
                        if sort.column == column {
                            match sort.direction {
                                SortDirection::Ascending => "Ascending",
                                SortDirection::Descending => "Descending",
                            }
                        } else {
                            "Not sorted"
                        }
                        .into(),
                    ),
                    enabled: true,
                });
            }
            controls.push(AxChild {
                id: RANDOM_NAVIGATION.into(),
                role: AxRole::CheckBox,
                label: "Random sample navigation".into(),
                value: Some(
                    if self.random_navigation.enabled() {
                        "On"
                    } else {
                        "Off"
                    }
                    .into(),
                ),
                enabled: true,
            });
            controls.push(AxChild {
                id: NAME_VIEW_MODE.into(),
                role: AxRole::Button,
                label: "Sample name view".into(),
                value: Some(self.sample_name_view_mode.label().into()),
                enabled: true,
            });
            let browser_focus_id = self.browser_focus_id();
            for path in &ax.visible_samples {
                let Some(row) = self.view_model.rows().iter().find(|row| {
                    self.entries
                        .get(row.entry_index)
                        .is_some_and(|entry| &entry.path == path)
                }) else {
                    continue;
                };
                let Some(entry) = self.entries.get(row.entry_index) else {
                    continue;
                };
                let (label, _) = self.sample_name_display(&entry.relative_path);
                let selected =
                    self.sample_row_is_selected_with_focus(&row.id, browser_focus_id.as_ref());
                let value = format!(
                    "{}, {}",
                    if selected { "Selected" } else { "Not selected" },
                    yield_core::format_duration(entry.duration_seconds)
                );
                controls.push(AxChild {
                    id: sample_id(path),
                    role: AxRole::Row,
                    label,
                    value: Some(value),
                    enabled: !super::tag_filter::scope_pending(self),
                });
            }
            controls
        };
        let controls_changed = *ax.last_controls.borrow() != controls;
        if controls_changed {
            if let Err(error) = ax.registry.borrow_mut().reconcile(controls.clone()) {
                eprintln!("browser accessibility reconcile: {error:?}");
                return;
            }
            *ax.last_controls.borrow_mut() = controls;
        }
        let registry = ax.registry.borrow();
        let volume = super::volume::quantize_volume(self.volume);
        if registry.contains(VOLUME) && (controls_changed || ax.last_volume.get() != Some(volume)) {
            if registry
                .set_slider(
                    VOLUME,
                    AxSlider {
                        value: f64::from(volume),
                        min: 0.0,
                        max: 1.0,
                    },
                )
                .is_ok()
            {
                ax.last_volume.set(Some(volume));
            }
        } else if !registry.contains(VOLUME) {
            ax.last_volume.set(None);
        }
        for (id, focus) in [
            (BEAT_COUNT, self.beat_count_input.read(cx).focus_handle()),
            (
                PLAYMARK_TIMING,
                self.playmark_timing_input.read(cx).focus_handle(),
            ),
            (VOLUME, self.volume_focus_handle.clone()),
            (WAVEFORM, self.waveform_focus_handle.clone()),
        ] {
            if registry.contains(id) {
                let _ =
                    registry.set_focused(id, focus.is_focused(window) && window.is_window_active());
            }
        }
        for (id, focus) in [
            (ADD_SOURCE, &ax.add_focus),
            (FOLDER_SCOPE, &ax.scope_focus),
            (REFRESH_SOURCE, &ax.refresh_focus),
        ] {
            if registry.contains(id) {
                let _ =
                    registry.set_focused(id, focus.is_focused(window) && window.is_window_active());
            }
        }
        if registry.contains(SEARCH) {
            let search_focus = self.search_input.read(cx).focus_handle();
            let _ = registry.set_focused(
                SEARCH,
                search_focus.is_focused(window) && window.is_window_active(),
            );
        }
        if registry.contains(TAG_FILTER_INPUT) {
            let focus = self.tag_filter_input.read(cx).focus_handle();
            let _ = registry.set_focused(
                TAG_FILTER_INPUT,
                focus.is_focused(window) && window.is_window_active(),
            );
        }
        for (id, focus) in &ax.control_focus {
            if registry.contains(id) {
                let _ =
                    registry.set_focused(id, focus.is_focused(window) && window.is_window_active());
            }
        }
    }

    pub(super) fn ax_focus(&self, id: &str) -> Option<FocusHandle> {
        self.accessibility.as_ref()?.focus_handle(id)
    }

    pub(super) fn ax_marker(&self, id: impl Into<String>) -> Option<gpui::AnyElement> {
        let registry = self.accessibility.as_ref()?.registry.clone();
        let id = id.into();
        Some(
            canvas(
                move |bounds, _, _| {
                    let _ = registry.borrow().set_frame(
                        &id,
                        AxFrame {
                            x: f64::from(f32::from(bounds.origin.x)),
                            y: f64::from(f32::from(bounds.origin.y)),
                            width: f64::from(f32::from(bounds.size.width)),
                            height: f64::from(f32::from(bounds.size.height)),
                        },
                    );
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
            .into_any_element(),
        )
    }

    pub(super) fn ax_frame_setter(
        &self,
        id: impl Into<String>,
    ) -> Option<Rc<dyn Fn(gpui::Bounds<gpui::Pixels>)>> {
        let registry = self.accessibility.as_ref()?.registry.clone();
        let id = id.into();
        Some(Rc::new(move |bounds| {
            let _ = registry.borrow().set_frame(
                &id,
                AxFrame {
                    x: f64::from(f32::from(bounds.origin.x)),
                    y: f64::from(f32::from(bounds.origin.y)),
                    width: f64::from(f32::from(bounds.size.width)),
                    height: f64::from(f32::from(bounds.size.height)),
                },
            );
        }))
    }
}

impl Drop for BrowserAccessibility {
    fn drop(&mut self) {
        let _ = self.registry.borrow_mut().reconcile(Vec::new());
    }
}
