#![deny(missing_docs)]
#![deny(warnings)]

//! Browser state, scan lifecycle, selection, and keyboard navigation.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use gpui::{
    App, AppContext, BorrowAppContext, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent,
    PathPromptOptions, ScrollHandle, ScrollStrategy, Task, UniformListScrollHandle, Window,
};
use reson::{PlaybackRequestId, PlaybackRuntime};
use wavecrate_core::{
    AudioFile, AudioFileId, BrowserSelection, BrowserViewModel, FolderScope, FolderTree,
    FolderTreeRow, PlaybackLoopPolicy, PlaybackNavigationHistory, SortColumn, WaveformViewport,
};
use wavecrate_design::{Motion, Theme, load_theme_file};
use wavecrate_library::sample_sources::{SampleSource, SourceId};
use wavecrate_media::{
    WaveformPreview, enumerate_audio_files_with_cancel, load_waveform_preview_with_cancel,
};
use wavecrate_session::{PreferencePatch, RegistryRevision, RequestId};

use crate::beat_count_input::{BeatCountChanged, BeatCountInput, DEFAULT_BEAT_COUNT};
use crate::search_input::{SearchChanged, SearchInput, SearchInputOptions, SearchInputRejected};
use crate::theme::ActiveTheme;

mod audio_output;
mod catalog;
mod catalog_metadata;
mod context_menu;
mod curation;
mod directory_open;
mod display_labels;
mod file_drag;
mod handoff;
mod history;
mod motion;
mod normalized_audition;
mod preferences;
mod shortcut_help;
mod sidebar;
mod source_recovery;
mod source_registry;
mod tag_chips;
mod tag_completion;
mod tag_dictionary;
mod tag_filter;
mod tag_library;
mod tag_pager;
mod tags;
mod transport;
mod view;
mod volume;
mod waveform;
mod waveform_detail;
mod waveform_scrollbar;

/// Install the app-owned source registry and its application-quit shutdown observer.
pub(crate) fn install_source_registry(cx: &mut App) {
    cx.set_global(source_registry::SourceRegistryGlobal::start());
    let quit_subscription = cx.on_app_quit(|cx| {
        if cx.has_global::<source_registry::SourceRegistryGlobal>() {
            cx.global_mut::<source_registry::SourceRegistryGlobal>()
                .shutdown();
        }
        async {}
    });
    cx.global_mut::<source_registry::SourceRegistryGlobal>()
        .retain_quit_subscription(quit_subscription);
}

#[derive(Clone, Debug)]
struct SourceSession {
    source_id: Option<SourceId>,
    root: PathBuf,
    label: String,
    entries: Vec<AudioFile>,
    tree: FolderTree,
    selected_folder: PathBuf,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    selected_audio_id: Option<AudioFileId>,
    catalog_facts: HashMap<PathBuf, catalog::CatalogFileFacts>,
    catalog_lifecycle_generation: Option<u64>,
    last_error: Option<String>,
}

#[derive(Clone, Debug)]
struct PendingRegistryMutation {
    root: PathBuf,
    revision: Option<RegistryRevision>,
}

impl SourceSession {
    fn new(root: PathBuf) -> Self {
        let label = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| root.display().to_string());
        Self {
            source_id: None,
            root,
            label,
            entries: Vec::new(),
            tree: FolderTree::default(),
            selected_folder: PathBuf::from("."),
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            selected_audio_id: None,
            catalog_facts: HashMap::new(),
            catalog_lifecycle_generation: None,
            last_error: None,
        }
    }

    fn from_source(source: &SampleSource) -> Self {
        let mut session = Self::new(source.root.clone());
        session.source_id = Some(source.id.clone());
        session
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleClickIntent {
    Focus,
    ReplaceRange,
    AddRange,
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionOrigin {
    Intentional,
    Batch,
    Passive,
}

fn should_autoplay_selection(enabled: bool, origin: SelectionOrigin) -> bool {
    enabled && matches!(origin, SelectionOrigin::Intentional)
}

fn selection_origin_for_click(
    intent: SampleClickIntent,
    suppress_autoplay: bool,
) -> SelectionOrigin {
    match intent {
        SampleClickIntent::Focus if !suppress_autoplay => SelectionOrigin::Intentional,
        SampleClickIntent::Focus => SelectionOrigin::Passive,
        SampleClickIntent::ReplaceRange
        | SampleClickIntent::AddRange
        | SampleClickIntent::Toggle => SelectionOrigin::Batch,
    }
}

fn selection_origin_for_navigation(extend: bool, suppress_autoplay: bool) -> SelectionOrigin {
    if extend || suppress_autoplay {
        SelectionOrigin::Batch
    } else {
        SelectionOrigin::Intentional
    }
}

fn sample_click_intent(shift: bool, secondary: bool) -> SampleClickIntent {
    match (shift, secondary) {
        (true, true) => SampleClickIntent::AddRange,
        (true, false) => SampleClickIntent::ReplaceRange,
        (false, true) => SampleClickIntent::Toggle,
        (false, false) => SampleClickIntent::Focus,
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
    waveform_focus_handle: FocusHandle,
    volume_focus_handle: FocusHandle,
    search_input: gpui::Entity<SearchInput>,
    _search_subscription: gpui::Subscription,
    beat_count_input: gpui::Entity<BeatCountInput>,
    _beat_count_subscription: gpui::Subscription,
    tag_input: gpui::Entity<SearchInput>,
    _tag_subscription: gpui::Subscription,
    _tag_rejected_subscription: gpui::Subscription,
    tag_filter_input: gpui::Entity<SearchInput>,
    _tag_filter_subscription: gpui::Subscription,
    _tag_filter_rejected_subscription: gpui::Subscription,
    root: Option<PathBuf>,
    entries: Vec<AudioFile>,
    source_sessions: Vec<SourceSession>,
    active_source: Option<usize>,
    pending_source_label: Option<String>,
    pending_source_return_index: Option<usize>,
    pending_source_return_root: Option<PathBuf>,
    folder_tree: Option<FolderTree>,
    folder_rows_cache: Vec<FolderTreeRow>,
    selected_folder: PathBuf,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    restore_selection_id: Option<AudioFileId>,
    restore_selection_position: Option<usize>,
    view_model: BrowserViewModel,
    sample_name_view_mode: wavecrate_core::SampleNameViewMode,
    selected: Option<usize>,
    selection: BrowserSelection,
    browser_focus_override: Option<AudioFileId>,
    browser_focus_detached: bool,
    context_menu: Option<context_menu::ContextMenuState>,
    waveform: Option<WaveformPreview>,
    waveform_error: Option<String>,
    waveform_render_cache: waveform::WaveformRenderCache,
    waveform_detail: waveform_detail::WaveformDetailState,
    bpm_snap_enabled: bool,
    beat_guides_enabled: bool,
    beat_guide_count: u8,
    normalized_audition: normalized_audition::NormalizedAuditionState,
    waveform_viewport: WaveformViewport,
    waveform_zoom_anchor_ratio: f64,
    waveform_play_region: Option<waveform::WaveformPlayRegion>,
    waveform_region_drag: Option<waveform::WaveformRegionDrag>,
    waveform_pan_drag: Option<waveform::WaveformPanDrag>,
    waveform_scrollbar_drag: Option<waveform_scrollbar::WaveformScrollbarDrag>,
    scan_status: String,
    playback_status: String,
    volume: f32,
    volume_drag: Option<volume::VolumeDrag>,
    volume_runtime_pending: Option<f32>,
    volume_runtime_retry_count: u8,
    volume_status: Option<String>,
    scan_generation: u64,
    waveform_generation: u64,
    playback_epoch: u64,
    playing_path: Option<PathBuf>,
    playback_policy: PlaybackLoopPolicy,
    accepted_playback_intent: Option<transport::PlaybackIntent>,
    autoplay_selection: bool,
    playback_history: PlaybackNavigationHistory,
    latest_playback_request_id: Option<PlaybackRequestId>,
    pending_loop_retarget: Option<transport::PendingLoopRetarget>,
    pending_progress_poll: Option<transport::PendingProgressPoll>,
    playback_cursor_pending: Option<f32>,
    playback_cursor_sample: Option<transport::PlaybackCursorSample>,
    playback_cursor_interpolation: Option<transport::PlaybackCursorInterpolation>,
    scan_cancel: Option<Arc<AtomicBool>>,
    scan_task: Option<Task<()>>,
    source_registry_task: Option<Task<()>>,
    preferences_task: Option<Task<()>>,
    source_registry_loaded: bool,
    source_registry_revision: RegistryRevision,
    source_registry_persisted_revision: RegistryRevision,
    source_registry_known_roots: BTreeSet<PathBuf>,
    source_registry_pending_adds: HashMap<RequestId, PendingRegistryMutation>,
    source_registry_pending_removes: HashMap<RequestId, PendingRegistryMutation>,
    source_registry_removed_roots: BTreeSet<PathBuf>,
    source_registry_status: String,
    catalog_next_generation: u64,
    catalog_activation: Option<catalog::CatalogActivation>,
    catalog_pending_command: Option<catalog::CatalogCommandIntent>,
    catalog_pending_removes: HashMap<SourceId, catalog::CatalogCommandIntent>,
    catalog_projection_task: Option<Task<()>>,
    catalog_projection_cancel: Option<Arc<AtomicBool>>,
    catalog_metadata: catalog_metadata::CatalogMetadataState,
    display_labels: display_labels::DisplayLabelsState,
    source_recovery: source_recovery::SourceRecoveryState,
    curation_state: curation::CurationState,
    tag_editor: tags::TagEditorState,
    tag_completion: tag_completion::TagCompletionState,
    tag_dictionary: tag_dictionary::TagDictionaryState,
    tag_library: tag_library::TagLibraryState,
    tag_filter: tag_filter::TagFilterState,
    tag_chips: tag_chips::TagChipsState,
    collection_filter_counts: [usize; wavecrate_core::COLLECTION_COUNT],
    preferences: preferences::PreferenceState,
    waveform_cancel: Option<Arc<AtomicBool>>,
    waveform_task: Option<Task<()>>,
    normalized_audition_task: Option<Task<()>>,
    normalized_audition_queued: Option<normalized_audition::PendingNormalizedAudition>,
    normalized_audition_deferred_play: Option<normalized_audition::PendingNormalizedAudition>,
    normalized_audition_pending_loop_target: Option<bool>,
    normalized_gain_runtime_pending: Option<normalized_audition::PendingNormalizedGain>,
    waveform_pending: bool,
    motion_started_at: Option<Instant>,
    motion_clock_origin: Instant,
    focus_target: Option<motion::FocusTarget>,
    focus_trigger: motion::FocusTrigger,
    focus_started_at: Option<Instant>,
    sample_edge_transitions: Rc<RefCell<motion::ScrollEdgeTransitions>>,
    source_edge_transitions: Rc<RefCell<motion::ScrollEdgeTransitions>>,
    folder_edge_transitions: Rc<RefCell<motion::ScrollEdgeTransitions>>,
    theme_status: String,
    theme_load: ThemeLoadState,
    theme_task: Option<Task<()>>,
    theme_settings_open: bool,
    audio_settings_open: bool,
    shortcut_help: shortcut_help::ShortcutHelpState,
    audio_output_focus_handle: FocusHandle,
    audio_output_dropdown: Option<audio_output::AudioOutputDropdown>,
    audio_output_option_index: usize,
    audio_output_options_scroll_handle: ScrollHandle,
    audio_output_config: reson::AudioOutputConfig,
    audio_output_resolved: Option<reson::ResolvedOutput>,
    audio_output_hosts: Vec<reson::AudioHostSummary>,
    audio_output_devices: Vec<reson::AudioDeviceSummary>,
    audio_output_sample_rates: Vec<u32>,
    audio_output_status: String,
    audio_output_error: Option<String>,
    audio_output_options_generation: u64,
    audio_output_options_cancel: Option<Arc<AtomicBool>>,
    audio_output_options_task: Option<Task<()>>,
    audio_output_open_generation: u64,
    audio_output_open_task: Option<Task<()>>,
    audio_output_retiring: Option<PlaybackRuntime>,
    audio_output_applied_config: Option<reson::AudioOutputConfig>,
    audio_output_resume: Option<transport::PlaybackIntent>,
    audio_output_resume_epoch: u64,
    audio_output_resume_generation: u64,
    audio_output_dirty: bool,
    playback_waiting_for_preferences: bool,
    playback_init_intent: Option<transport::PlaybackIntent>,
    playback_init_epoch: u64,
    playback_init_generation: u64,
    playback_monitor_task: Option<Task<()>>,
    playback_monitor_generation: u64,
    playback_monitor_running: bool,
    playback: Option<PlaybackRuntime>,
    file_drag: file_drag::FileDragState,
    directory_open: directory_open::DirectoryOpenState,
    handoff_task: Option<Task<()>>,
    handoff_generation: u64,
    list_scroll_handle: UniformListScrollHandle,
    source_scroll_handle: UniformListScrollHandle,
    folder_scroll_handle: UniformListScrollHandle,
}

impl Browser {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(SearchInput::new);
        let search_subscription =
            cx.subscribe(&search_input, |view, input, _: &SearchChanged, cx| {
                let previous_id = view.browser_focus_id();
                let previous_position = view.selected_visible_position();
                view.reset_browser_focus_state();
                view.context_menu = None;
                let query = input.read(cx).content().to_owned();
                view.invalidate_file_handoff();
                view.view_model.set_query(&view.entries, query);
                view.refresh_collection_filter_counts();
                view.reconcile_selection();
                view.restore_explicit_projection_focus(previous_id, previous_position, cx);
            });
        let beat_count_input = cx.new(BeatCountInput::new);
        let beat_count_subscription = cx.subscribe(
            &beat_count_input,
            |view, _input, event: &BeatCountChanged, cx| {
                view.beat_guide_count = event.count;
                cx.notify();
            },
        );
        let tag_input = cx.new(|cx| SearchInput::new_with_options(cx, SearchInputOptions::tags()));
        let tag_subscription = cx.subscribe(&tag_input, |view, input, _: &SearchChanged, cx| {
            let content = input.read(cx).content().to_owned();
            tags::on_changed(view, &content, cx);
            tag_completion::on_changed(view, &content, cx);
        });
        let tag_rejected_subscription =
            cx.subscribe(&tag_input, |view, _input, _: &SearchInputRejected, cx| {
                tags::on_rejected(view, cx);
                tag_completion::on_rejected(view, cx);
            });
        let tag_filter_input =
            cx.new(|cx| SearchInput::new_with_options(cx, SearchInputOptions::tag_filter()));
        let tag_filter_subscription =
            cx.subscribe(&tag_filter_input, |view, input, _: &SearchChanged, cx| {
                let content = input.read(cx).content().to_owned();
                tag_filter::on_changed(view, &content, cx);
            });
        let tag_filter_rejected_subscription = cx.subscribe(
            &tag_filter_input,
            |view, _input, _: &SearchInputRejected, cx| {
                tag_filter::on_rejected(view, cx);
            },
        );
        let motion_settings = Motion::default();
        let mut browser = Self {
            focus_handle: cx.focus_handle(),
            waveform_focus_handle: cx.focus_handle(),
            volume_focus_handle: cx.focus_handle(),
            search_input,
            _search_subscription: search_subscription,
            beat_count_input,
            _beat_count_subscription: beat_count_subscription,
            tag_input,
            _tag_subscription: tag_subscription,
            _tag_rejected_subscription: tag_rejected_subscription,
            tag_filter_input,
            _tag_filter_subscription: tag_filter_subscription,
            _tag_filter_rejected_subscription: tag_filter_rejected_subscription,
            root: None,
            entries: Vec::new(),
            source_sessions: Vec::new(),
            active_source: None,
            pending_source_label: None,
            pending_source_return_index: None,
            pending_source_return_root: None,
            folder_tree: None,
            folder_rows_cache: Vec::new(),
            selected_folder: PathBuf::from("."),
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            restore_selection_id: None,
            restore_selection_position: None,
            view_model: BrowserViewModel::new(&[]),
            sample_name_view_mode: wavecrate_core::SampleNameViewMode::default(),
            selected: None,
            selection: BrowserSelection::new(),
            browser_focus_override: None,
            browser_focus_detached: false,
            context_menu: None,
            waveform: None,
            waveform_error: None,
            waveform_render_cache: waveform::WaveformRenderCache::default(),
            waveform_detail: waveform_detail::WaveformDetailState::default(),
            bpm_snap_enabled: false,
            beat_guides_enabled: false,
            beat_guide_count: DEFAULT_BEAT_COUNT,
            normalized_audition: normalized_audition::NormalizedAuditionState::default(),
            waveform_viewport: WaveformViewport::full(),
            waveform_zoom_anchor_ratio: 0.5,
            waveform_play_region: None,
            waveform_region_drag: None,
            waveform_pan_drag: None,
            waveform_scrollbar_drag: None,
            scan_status: "Choose a folder or pass one on the command line".to_string(),
            playback_status: "Ready".to_string(),
            volume: volume::DEFAULT_VOLUME,
            volume_drag: None,
            volume_runtime_pending: None,
            volume_runtime_retry_count: 0,
            volume_status: None,
            scan_generation: 0,
            waveform_generation: 0,
            playback_epoch: 0,
            playing_path: None,
            playback_policy: PlaybackLoopPolicy::default(),
            accepted_playback_intent: None,
            autoplay_selection: true,
            playback_history: PlaybackNavigationHistory::default(),
            latest_playback_request_id: None,
            pending_loop_retarget: None,
            pending_progress_poll: None,
            playback_cursor_pending: None,
            playback_cursor_sample: None,
            playback_cursor_interpolation: None,
            scan_cancel: None,
            scan_task: None,
            source_registry_task: None,
            preferences_task: None,
            source_registry_loaded: false,
            source_registry_revision: RegistryRevision::default(),
            source_registry_persisted_revision: RegistryRevision::default(),
            source_registry_known_roots: BTreeSet::new(),
            source_registry_pending_adds: HashMap::new(),
            source_registry_pending_removes: HashMap::new(),
            source_registry_removed_roots: BTreeSet::new(),
            source_registry_status: "Loading sources…".to_string(),
            catalog_next_generation: 0,
            catalog_activation: None,
            catalog_pending_command: None,
            catalog_pending_removes: HashMap::new(),
            catalog_projection_task: None,
            catalog_projection_cancel: None,
            catalog_metadata: catalog_metadata::CatalogMetadataState::default(),
            display_labels: display_labels::DisplayLabelsState::default(),
            source_recovery: source_recovery::SourceRecoveryState::default(),
            curation_state: curation::CurationState::default(),
            tag_editor: tags::TagEditorState::default(),
            tag_completion: tag_completion::TagCompletionState::default(),
            tag_dictionary: tag_dictionary::TagDictionaryState::default(),
            tag_library: tag_library::TagLibraryState::default(),
            tag_filter: tag_filter::TagFilterState::default(),
            tag_chips: tag_chips::TagChipsState::default(),
            collection_filter_counts: [0; wavecrate_core::COLLECTION_COUNT],
            preferences: preferences::PreferenceState::new(),
            waveform_cancel: None,
            waveform_task: None,
            normalized_audition_task: None,
            normalized_audition_queued: None,
            normalized_audition_deferred_play: None,
            normalized_audition_pending_loop_target: None,
            normalized_gain_runtime_pending: None,
            waveform_pending: false,
            motion_started_at: None,
            motion_clock_origin: Instant::now(),
            focus_target: None,
            focus_trigger: motion::FocusTrigger::Pointer,
            focus_started_at: None,
            sample_edge_transitions: Rc::new(RefCell::new(motion::ScrollEdgeTransitions::new(
                motion_settings,
            ))),
            source_edge_transitions: Rc::new(RefCell::new(motion::ScrollEdgeTransitions::new(
                motion_settings,
            ))),
            folder_edge_transitions: Rc::new(RefCell::new(motion::ScrollEdgeTransitions::new(
                motion_settings,
            ))),
            theme_status: "Wavecrate Editorial Terminal".to_string(),
            theme_load: ThemeLoadState::default(),
            theme_task: None,
            theme_settings_open: false,
            audio_settings_open: false,
            shortcut_help: shortcut_help::ShortcutHelpState::new(cx),
            audio_output_focus_handle: cx.focus_handle(),
            audio_output_dropdown: None,
            audio_output_option_index: 0,
            audio_output_options_scroll_handle: ScrollHandle::new(),
            audio_output_config: reson::AudioOutputConfig::default(),
            audio_output_resolved: None,
            audio_output_hosts: Vec::new(),
            audio_output_devices: Vec::new(),
            audio_output_sample_rates: Vec::new(),
            audio_output_status: "Audio output idle".to_string(),
            audio_output_error: None,
            audio_output_options_generation: 0,
            audio_output_options_cancel: None,
            audio_output_options_task: None,
            audio_output_open_generation: 0,
            audio_output_open_task: None,
            audio_output_retiring: None,
            audio_output_applied_config: None,
            audio_output_resume: None,
            audio_output_resume_epoch: 0,
            audio_output_resume_generation: 0,
            audio_output_dirty: false,
            playback_waiting_for_preferences: false,
            playback_init_intent: None,
            playback_init_epoch: 0,
            playback_init_generation: 0,
            playback_monitor_task: None,
            playback_monitor_generation: 0,
            playback_monitor_running: false,
            playback: None,
            file_drag: file_drag::FileDragState::default(),
            directory_open: directory_open::DirectoryOpenState::default(),
            handoff_task: None,
            handoff_generation: 0,
            list_scroll_handle: UniformListScrollHandle::new(),
            source_scroll_handle: UniformListScrollHandle::new(),
            folder_scroll_handle: UniformListScrollHandle::new(),
        };
        browser.start_source_registry_poll(cx);
        browser.start_preferences_poll(cx);
        browser
    }

    fn selected_id(&self) -> Option<AudioFileId> {
        self.selected
            .and_then(|index| self.entries.get(index))
            .map(AudioFile::stable_id)
    }

    fn bpm_snap_settings(&self) -> waveform::BpmSnapSettings {
        waveform::BpmSnapSettings {
            enabled: self.bpm_snap_enabled,
            beat_count: self.beat_guide_count,
            audio_duration_seconds: self
                .waveform
                .as_ref()
                .map(|preview| preview.duration_seconds)
                .unwrap_or_default(),
            constrain_to_audio: true,
        }
    }

    /// Return the browser row focus, keeping it independent from the loaded waveform selection.
    fn browser_focus_id(&self) -> Option<AudioFileId> {
        browser_focus_from_state(
            self.browser_focus_override.as_ref(),
            self.browser_focus_detached,
            self.selected_id(),
        )
    }

    fn clear_browser_focus_override(&mut self) {
        self.browser_focus_override = None;
    }

    fn reset_browser_focus_state(&mut self) {
        self.browser_focus_override = None;
        self.browser_focus_detached = false;
    }

    fn set_browser_focus_override(&mut self, id: AudioFileId) {
        self.browser_focus_override = Some(id);
    }

    fn detach_browser_focus(&mut self) {
        self.browser_focus_override = None;
        self.browser_focus_detached = true;
    }

    /// Resolve browser focus to the current scan index without changing the loaded preview.
    fn browser_focus_index(&self) -> Option<usize> {
        let focus_id = self.browser_focus_id()?;
        self.entries
            .iter()
            .position(|entry| entry.stable_id() == focus_id)
    }

    fn clear_reveal_for_focus_change(&mut self, next_id: &AudioFileId) {
        if reveal_should_clear_for_focus_change(self.view_model.reveal_id(), next_id)
            && self.view_model.clear_reveal(&self.entries)
        {
            self.reconcile_selection();
        }
    }

    fn cancel_waveform_load(&mut self) {
        self.waveform_detail.cancel();
        if let Some(cancellation) = self.waveform_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.waveform_task.take();
    }

    fn visible_selection_ids(&self) -> Vec<AudioFileId> {
        self.view_model
            .rows()
            .iter()
            .map(|row| row.id.clone())
            .collect()
    }

    fn reconcile_selection(&mut self) {
        if tag_filter::scope_pending(self) {
            return;
        }
        let visible_ids = self.visible_selection_ids();
        if self.selection.reconcile(&visible_ids) {
            self.invalidate_file_handoff();
            tag_chips::invalidate(self);
        }
    }

    /// Highlight batch members, falling back to the focused sample for ordinary navigation.
    fn sample_row_is_selected_with_focus(
        &self,
        id: &AudioFileId,
        browser_focus_id: Option<&AudioFileId>,
    ) -> bool {
        self.selection.contains(id) || (self.selection.is_empty() && browser_focus_id == Some(id))
    }

    fn selected_count_label(&self) -> Option<String> {
        (!self.selection.is_empty()).then(|| format!("{} selected", self.selection.len()))
    }

    fn selected_visible_position(&self) -> Option<usize> {
        self.browser_focus_id()
            .as_ref()
            .and_then(|id| self.view_model.visible_position_for(id))
    }

    fn select_entry_from_click(
        &mut self,
        index: usize,
        event: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        if tag_filter::scope_pending(self) {
            return;
        }
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let target_id = entry.stable_id();
        let focused_id = self.browser_focus_id();
        let modifiers = event.modifiers();
        let intent = sample_click_intent(modifiers.shift, modifiers.platform || modifiers.control);
        let origin = selection_origin_for_click(intent, modifiers.alt || modifiers.function);
        self.invalidate_file_handoff();
        match intent {
            SampleClickIntent::Focus => self.selection.focus_pointer(&target_id),
            SampleClickIntent::ReplaceRange | SampleClickIntent::AddRange => {
                let additive = matches!(intent, SampleClickIntent::AddRange);
                self.selection.extend_range(
                    self.view_model.rows().iter().map(|row| &row.id),
                    focused_id.as_ref(),
                    &target_id,
                    additive,
                );
            }
            SampleClickIntent::Toggle => {
                self.selection.toggle(&target_id, focused_id.as_ref());
            }
        }
        tag_chips::invalidate(self);
        self.select_entry(index, Some(motion::FocusTrigger::Pointer), origin, cx);
    }

    fn restore_projection_focus(
        &mut self,
        previous_id: Option<AudioFileId>,
        previous_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if tag_filter::scope_pending(self) {
            self.start_source_registry_poll(cx);
            return;
        }
        tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        let next = self
            .view_model
            .restore_focus(previous_id.as_ref(), previous_position);
        match next {
            Some(index) => self.select_entry(index, None, SelectionOrigin::Passive, cx),
            None => {
                if self.selected.is_some() {
                    self.stop_playback();
                    self.selected = None;
                    self.cancel_waveform_load();
                    self.waveform_pending = false;
                    self.waveform = None;
                    self.waveform_error = None;
                    self.invalidate_waveform_generation();
                }
                self.playback_policy.clear_file();
                if let Some(index) = self.active_source
                    && let Some(session) = self.source_sessions.get_mut(index)
                {
                    session.selected_audio_id = None;
                }
                self.reset_focus();
                cx.notify();
            }
        }
    }

    /// Restore focus after an explicit projection change, recording its priority while a
    /// tag-filter result is still pending.
    fn restore_explicit_projection_focus(
        &mut self,
        previous_id: Option<AudioFileId>,
        previous_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if tag_filter::scope_pending(self) {
            tag_filter::preserve_focus_for_projection(
                self,
                previous_id.clone(),
                previous_position,
                tag_filter::FilterOrigin::Explicit,
            );
        }
        self.restore_projection_focus(previous_id, previous_position, cx);
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
        self.invalidate_file_handoff();
        self.reset_browser_focus_state();
        self.context_menu = None;
        self.deactivate_catalog_source(cx);
        if source_index != self.active_source {
            self.selection.clear();
            self.reset_collection_filter();
        }
        self.reset_focus();
        self.reset_scroll_edges();
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
        self.clear_waveform_region();
        if source_index != self.active_source {
            self.playback_policy.clear_file();
        }
        let pending_source_return_index = if source_index.is_none() {
            self.active_source.or(self.pending_source_return_index)
        } else {
            None
        };
        self.pending_source_return_root = pending_source_return_index
            .and_then(|index| self.source_sessions.get(index))
            .map(|session| session.root.clone());
        self.pending_source_return_index = pending_source_return_index;
        self.active_source = source_index;
        self.pending_source_label = source_index.is_none().then(|| source_label(&root));
        self.root = Some(root.clone());
        self.entries.clear();
        self.view_model.set_entries(&self.entries);
        self.refresh_collection_filter_counts();
        self.selected = None;
        self.cancel_waveform_load();
        self.waveform_pending = false;
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
        self.invalidate_waveform_generation();
        self.scan_generation = self.scan_generation.wrapping_add(1);
        let generation = self.scan_generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancellation));
        self.scan_status = format!("Scanning {}…", root.display());
        cx.notify();

        let scan_root = root.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { enumerate_audio_files_with_cancel(&root, Some(&cancellation)) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !scan_completion_is_current(generation, view.scan_generation) {
                    return;
                }
                view.scan_cancel = None;
                match result {
                    Ok(scan) => view.apply_scan_result(scan, source_index, cx),
                    Err(error) => view.apply_scan_error(source_index, scan_root, error, cx),
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
        let register_source = self.source_sessions[source_index].source_id.is_none();
        let session = &mut self.source_sessions[source_index];
        session.root = root.clone();
        session.label = source_label(&root);
        session.entries = entries.clone();
        session.tree = tree.clone();
        session.catalog_facts.clear();
        session.selected_folder = state.selected.clone();
        session.expanded_folders = state.expanded.clone();
        session.folder_scope = previous_scope;
        session.last_error = None;

        self.active_source = Some(source_index);
        self.pending_source_label = None;
        self.pending_source_return_index = None;
        self.pending_source_return_root = None;
        self.root = Some(root.clone());
        self.entries = entries;
        self.folder_tree = Some(tree);
        self.selected_folder = state.selected;
        self.expanded_folders = state.expanded;
        self.rebuild_folder_rows_cache();
        self.folder_scope = previous_scope;
        tag_filter::preserve_focus_for_projection(
            self,
            self.restore_selection_id
                .clone()
                .or(previous_selected.clone()),
            self.restore_selection_position,
            tag_filter::FilterOrigin::PassiveMetadata,
        );
        self.view_model.set_entries(&self.entries);
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        tag_filter::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.scan_status = format!(
            "{count} audio file{} · {} skipped",
            if count == 1 { "" } else { "s" },
            skipped_entries
        );
        if register_source {
            self.submit_source_add(root.clone(), cx);
        }
        let restore_id = self.restore_selection_id.take().or(previous_selected);
        let restore_position = self.restore_selection_position.take();
        self.restore_projection_focus(restore_id, restore_position, cx);
    }

    fn apply_scan_error(
        &mut self,
        source_index: Option<usize>,
        scan_root: PathBuf,
        error: String,
        cx: &mut Context<Self>,
    ) {
        let source_index =
            scan_source_index_for_error(&self.source_sessions, &scan_root, source_index);
        let mut scan_session_index = source_index;
        if scan_session_index.is_none() {
            scan_session_index = self
                .source_sessions
                .iter()
                .position(|session| session.root == scan_root);
        }
        if source_index.is_none()
            && scan_session_index
                .is_some_and(|index| self.source_sessions[index].source_id.is_none())
        {
            self.source_sessions
                .remove(scan_session_index.take().expect("checked"));
        }
        let pending_return_root = self.pending_source_return_root.take();
        let pending_return_index = self.pending_source_return_index.take();
        let fallback_index = scan_session_index
            .or_else(|| {
                pending_return_root.and_then(|root| {
                    self.source_sessions
                        .iter()
                        .position(|session| session.root == root)
                })
            })
            .or_else(|| pending_return_index.filter(|index| *index < self.source_sessions.len()));
        let is_refresh = source_index.is_some()
            || fallback_index.is_some_and(|index| self.source_sessions[index].source_id.is_some());
        let Some(index) = fallback_index.filter(|index| *index < self.source_sessions.len()) else {
            self.active_source = None;
            self.pending_source_label = None;
            self.root = None;
            self.entries.clear();
            self.folder_tree = None;
            self.folder_rows_cache.clear();
            self.view_model.set_entries(&self.entries);
            tag_filter::reinstall_after_projection(self, cx);
            self.reset_collection_filter();
            self.scan_status = error;
            self.selected = None;
            self.playback_policy.clear_file();
            self.waveform_pending = false;
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
        self.pending_source_return_root = None;
        self.pending_source_label = None;
        self.root = Some(self.source_sessions[index].root.clone());
        self.entries = self.source_sessions[index].entries.clone();
        self.folder_tree = Some(self.source_sessions[index].tree.clone());
        self.selected_folder = self.source_sessions[index].selected_folder.clone();
        self.expanded_folders = self.source_sessions[index].expanded_folders.clone();
        self.rebuild_folder_rows_cache();
        self.folder_scope = self.source_sessions[index].folder_scope;
        tag_filter::preserve_focus_for_projection(
            self,
            self.restore_selection_id.clone(),
            self.restore_selection_position,
            tag_filter::FilterOrigin::PassiveMetadata,
        );
        self.view_model.set_entries(&self.entries);
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        tag_filter::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
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

    pub(super) fn source_rows(&self) -> Vec<(usize, PathBuf, String, bool, Option<String>)> {
        let pending_add_roots: BTreeSet<&Path> = self
            .source_registry_pending_adds
            .values()
            .map(|mutation| mutation.root.as_path())
            .collect();
        self.source_sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                !self.source_registry_removed_roots.contains(&session.root)
                    && !(session.source_id.is_none()
                        && self.scan_cancel.is_some()
                        && self.root.as_deref() == Some(session.root.as_path())
                        && !pending_add_roots.contains(session.root.as_path()))
            })
            .map(|(index, session)| {
                (
                    index,
                    session.root.clone(),
                    session.label.clone(),
                    self.active_source == Some(index),
                    session.last_error.clone(),
                )
            })
            .collect()
    }

    pub(super) fn pending_source_row(&self) -> Option<(PathBuf, String, u64)> {
        self.pending_source_label
            .as_ref()
            .zip(self.root.as_ref())
            .map(|(label, root)| (root.clone(), label.clone(), self.scan_generation))
    }

    pub(super) fn source_scan_is_pending(&self, root: &Path) -> bool {
        (self.scan_cancel.is_some() && self.root.as_deref() == Some(root))
            || catalog::is_catalog_pending(self, root)
    }

    pub(super) fn motion_pending(&self) -> motion::PendingWork {
        motion::pending_work(
            self.scan_cancel.is_some() || catalog::is_catalog_busy(self),
            self.waveform_pending || self.waveform_detail.is_pending(),
            self.audio_output_open_task.is_some(),
            self.playback_init_intent.is_some(),
        )
    }

    pub(super) fn motion_frame(&mut self, motion: Motion, window: &Window) -> motion::MotionFrame {
        let now = Instant::now();
        let pending = self.motion_pending();
        let focus_elapsed = self
            .focus_started_at
            .map(|started_at| now.duration_since(started_at));
        let focus_envelope = focus_elapsed.map(|_| self.focus_trigger.envelope(motion));
        let focus_pending = focus_envelope
            .zip(focus_elapsed)
            .is_some_and(|(envelope, elapsed)| envelope.needs_animation_at(elapsed));
        let busy_elapsed = if pending.any() {
            let started_at = *self.motion_started_at.get_or_insert(now);
            now.duration_since(started_at)
        } else {
            self.motion_started_at = None;
            std::time::Duration::ZERO
        };

        let mut frame = motion::MotionFrame::sample(motion, busy_elapsed);
        if motion.enabled && self.playback_cursor_animation_pending(now) {
            window.request_animation_frame();
        }
        if let (Some(target), Some(elapsed), Some(envelope)) =
            (self.focus_target.as_ref(), focus_elapsed, focus_envelope)
        {
            frame.focus_alpha = envelope.alpha_at(elapsed);
            frame.focus_surface = target.surface();
        }
        if motion::should_schedule_frames(motion.enabled, pending, focus_pending) {
            window.request_animation_frame();
        }
        frame
    }

    fn motion_time_now(&self) -> std::time::Duration {
        Instant::now().duration_since(self.motion_clock_origin)
    }

    fn reset_scroll_edges(&mut self) {
        let now = self.motion_time_now();
        self.sample_edge_transitions
            .borrow_mut()
            .retarget(motion::ScrollEdges::default(), now);
        self.source_edge_transitions
            .borrow_mut()
            .retarget(motion::ScrollEdges::default(), now);
        self.folder_edge_transitions
            .borrow_mut()
            .retarget(motion::ScrollEdges::default(), now);
    }

    fn reset_focus(&mut self) {
        self.focus_target = None;
        self.focus_started_at = None;
    }

    fn start_focus(&mut self, target: motion::FocusTarget, trigger: motion::FocusTrigger) {
        self.focus_target = Some(target);
        self.focus_trigger = trigger;
        self.focus_started_at = Some(Instant::now());
    }

    fn focus_matches_sample(&self, path: &Path) -> bool {
        self.focus_target
            .as_ref()
            .is_some_and(|target| target.matches_sample(path))
    }

    fn focus_matches_source(&self, path: &Path) -> bool {
        self.focus_target
            .as_ref()
            .is_some_and(|target| target.matches_source(path))
    }

    fn focus_matches_folder(&self, path: &Path) -> bool {
        self.focus_target
            .as_ref()
            .is_some_and(|target| target.matches_folder(path))
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
        if !self.begin_catalog_activation(index, false, false, true, cx) {
            self.begin_source_scan(root.clone(), Some(index), cx);
        }
        self.start_focus(
            motion::FocusTarget::Source(root),
            motion::FocusTrigger::Pointer,
        );
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
        let root = self.source_sessions[index].root.clone();
        self.deactivate_catalog_for_session(index, cx);
        let has_registry_identity = self.source_sessions[index].source_id.is_some()
            || self
                .source_registry_pending_adds
                .values()
                .any(|mutation| mutation.root == root);
        if !self.submit_source_remove(root.clone(), cx) {
            cx.notify();
            return;
        }
        self.reset_focus();
        self.reset_scroll_edges();
        let was_active = self.active_source == Some(index);
        if !has_registry_identity {
            self.source_sessions.remove(index);
            if let Some(active) = self.active_source
                && active > index
            {
                self.active_source = Some(active - 1);
            }
        }
        if was_active {
            self.selection.clear();
        }
        let had_pending_scan = self.scan_cancel.is_some() && self.root.as_deref() == Some(&root);
        if had_pending_scan && let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        if had_pending_scan {
            self.selection.clear();
        }
        if had_pending_scan || was_active {
            self.scan_generation = self.scan_generation.wrapping_add(1);
            self.invalidate_waveform_generation();
            self.cancel_waveform_load();
            self.stop_playback();
        }
        self.pending_source_return_index = None;
        self.pending_source_return_root = None;
        if was_active {
            self.active_source = None;
            self.clear_active_source_projection(cx);
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
        if self.catalog_activation.is_some() && source_recovery::manual_refresh(self, cx) {
            return;
        }
        if !self.begin_catalog_activation(index, true, false, true, cx) {
            self.begin_source_scan(root, Some(index), cx);
        }
    }

    pub(super) fn toggle_folder_scope(&mut self, cx: &mut Context<Self>) {
        self.invalidate_file_handoff();
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
        self.invalidate_file_handoff();
        self.selection.clear();
        self.selected_folder = path.clone();
        self.update_active_folder_projection(cx);
        self.start_focus(
            motion::FocusTarget::Folder(path),
            motion::FocusTrigger::Pointer,
        );
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
        self.invalidate_file_handoff();
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.reset_browser_focus_state();
        self.context_menu = None;
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        if let Some(index) = self.active_source
            && let Some(session) = self.source_sessions.get_mut(index)
        {
            session.selected_folder = self.selected_folder.clone();
            session.expanded_folders = self.expanded_folders.clone();
            session.folder_scope = self.folder_scope;
            session.selected_audio_id = previous_id.clone();
        }
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
    }

    fn rebuild_folder_rows_cache(&mut self) {
        self.folder_rows_cache = self
            .folder_tree
            .as_ref()
            .map(|tree| tree.rows(&self.expanded_folders))
            .unwrap_or_default();
    }

    fn select_entry(
        &mut self,
        index: usize,
        trigger: Option<motion::FocusTrigger>,
        origin: SelectionOrigin,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_file_handoff();
        tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        self.reset_browser_focus_state();
        self.context_menu = None;
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if trigger.is_none()
            && self.focus_target.as_ref().is_some_and(|target| {
                matches!(target, motion::FocusTarget::Sample(_))
                    && !target.matches_sample(&entry.path)
            })
        {
            self.reset_focus();
        }
        if let Some(trigger) = trigger {
            self.start_focus(motion::FocusTarget::Sample(entry.path.clone()), trigger);
        }
        let previous_id = self.selected_id();
        let next_id = entry.stable_id();
        self.playback_policy
            .prepare(next_id.clone(), entry.playback_type);
        self.clear_reveal_for_focus_change(&next_id);
        let autoplay = should_autoplay_selection(self.autoplay_selection, origin);
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
        self.cancel_waveform_load();
        self.selected = Some(index);
        if let Some(source_index) = self.active_source
            && let Some(session) = self.source_sessions.get_mut(source_index)
        {
            session.selected_audio_id = Some(next_id.clone());
        }
        self.waveform = None;
        self.waveform_error = None;
        self.invalidate_waveform_generation();
        let generation = self.waveform_generation;
        self.waveform_pending = true;
        let path = entry.path;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.waveform_cancel = Some(Arc::clone(&cancellation));
        self.playback_status = "Ready".to_string();
        self.waveform_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_waveform_preview_with_cancel(&path, Some(&cancellation)) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.waveform_generation != generation {
                    return;
                }
                view.waveform_cancel = None;
                view.waveform_task = None;
                view.waveform_pending = false;
                match result {
                    Ok(preview) => view.waveform = Some(preview),
                    Err(error) => {
                        view.clear_waveform_region();
                        view.waveform_error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
        if autoplay {
            self.request_selection_autoplay(cx);
        }
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
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
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
        if self.theme_settings_open {
            self.audio_settings_open = false;
            self.audio_output_dropdown = None;
            self.cancel_audio_output_options();
        }
        cx.notify();
    }

    pub(super) fn toggle_motion(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let mut theme = crate::theme::active(cx).clone();
        theme.motion.enabled = !theme.motion.enabled;
        if !self.persist_motion_preference(theme.motion.enabled, cx) {
            return;
        }
        cx.set_global(ActiveTheme(Arc::new(theme)));
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        cx.notify();
    }

    fn toggle_autoplay(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let enabled = !self.autoplay_selection;
        if !self.persist_autoplay_preference(enabled, cx) {
            return;
        }
        self.autoplay_selection = enabled;
        if !enabled {
            self.cancel_pending_selection_autoplay();
        }
        self.playback_status = if self.autoplay_selection {
            "Autoplay enabled".to_string()
        } else {
            "Autoplay disabled".to_string()
        };
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

    fn toggle_loop_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_loop(cx);
    }

    fn toggle_bpm_snap_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bpm_snap_enabled = !self.bpm_snap_enabled;
        cx.notify();
    }

    fn toggle_beat_guides_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.beat_guides_enabled = !self.beat_guides_enabled;
        cx.notify();
    }

    fn reload_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
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
        if !self.preferences_editable() {
            self.reset_preferences(cx);
            return;
        }
        let theme = Theme::default();
        if !self.submit_preference_patch(
            PreferencePatch::ReplaceTheme {
                theme: theme.clone(),
                source_path: None,
            },
            cx,
        ) {
            return;
        }
        self.theme_load.invalidate();
        self.theme_task.take();
        cx.set_global(ActiveTheme(Arc::new(theme)));
        self.theme_status = "Wavecrate Editorial Terminal".to_string();
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        cx.notify();
    }

    fn reset_preferences(&mut self, cx: &mut Context<Self>) {
        if !self.submit_preference_patch(PreferencePatch::Reset, cx) {
            return;
        }
        self.theme_load.invalidate();
        self.theme_task.take();
        cx.set_global(ActiveTheme::default());
        self.theme_status = "Wavecrate Editorial Terminal".to_string();
        self.autoplay_selection = true;
        let previous_loop_mode = self.playback_policy.enabled();
        self.playback_policy.reset(false);
        let loop_mode_changed = previous_loop_mode != self.playback_policy.enabled();
        self.apply_preference_volume(volume::DEFAULT_VOLUME);
        if loop_mode_changed {
            self.apply_loop_mode_change(self.playback_policy.enabled(), cx);
        }
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        cx.notify();
    }

    fn load_theme(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
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
        if !self.theme_load.is_current(generation) {
            return;
        }
        if !self.preferences_editable() {
            self.theme_load.reject(generation);
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let patch = PreferencePatch::ReplaceTheme {
            theme: theme.clone(),
            source_path: Some(path.clone()),
        };
        if !self.submit_preference_patch(patch, cx) {
            self.theme_load.reject(generation);
            return;
        }
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

    fn move_selection(
        &mut self,
        direction: isize,
        extend: bool,
        additive: bool,
        suppress_autoplay: bool,
        cx: &mut Context<Self>,
    ) {
        if self.view_model.rows().is_empty() {
            return;
        }
        self.invalidate_file_handoff();
        let current = self
            .browser_focus_id()
            .as_ref()
            .and_then(|id| self.view_model.visible_position_for(id))
            .unwrap_or(0) as isize;
        let next_position =
            (current + direction).clamp(0, self.view_model.rows().len() as isize - 1) as usize;
        let next_row = self.view_model.rows()[next_position].clone();
        let next = next_row.entry_index;
        let next_id = next_row.id;
        let focused_id = self.browser_focus_id();
        if extend {
            self.selection.extend_range(
                self.view_model.rows().iter().map(|row| &row.id),
                focused_id.as_ref(),
                &next_id,
                additive,
            );
        } else {
            self.selection.move_anchor_to(&next_id);
        }
        if extend {
            tag_chips::invalidate(self);
            self.start_source_registry_poll(cx);
        }
        self.clear_reveal_for_focus_change(&next_id);
        let passive_focus = self.browser_focus_override.is_some();
        if Some(next) != self.selected || passive_focus {
            let origin = selection_origin_for_navigation(extend, suppress_autoplay);
            self.select_entry(next, Some(motion::FocusTrigger::Keyboard), origin, cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        self.invalidate_file_handoff();
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.reset_browser_focus_state();
        self.context_menu = None;
        self.view_model.toggle_sort(&self.entries, column);
        tag_chips::invalidate(self);
        self.reconcile_selection();
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
    }

    fn select_all_visible(&mut self, cx: &mut Context<Self>) {
        if tag_filter::scope_pending(self) {
            return;
        }
        self.invalidate_file_handoff();
        let visible_ids = self.visible_selection_ids();
        self.selection.select_all(&visible_ids);
        tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Toggle explicit membership for the focused row. This is the reference browser's `X`
    /// action; focused-row fallback still makes an empty explicit set auditionable as one item.
    fn toggle_focused_selection(&mut self, cx: &mut Context<Self>) {
        if tag_filter::scope_pending(self) {
            return;
        }
        let Some(focused_id) = self.browser_focus_id() else {
            return;
        };
        self.invalidate_file_handoff();
        self.selection.toggle(&focused_id, Some(&focused_id));
        tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let secondary = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
        let alt = event.keystroke.modifiers.alt;
        let function = event.keystroke.modifiers.function;
        let navigation_suppresses_autoplay = secondary || alt || function;
        let shift = event.keystroke.modifiers.shift;
        match shortcut_help::key_action(
            self.shortcut_help.is_open(),
            key,
            secondary,
            shift,
            alt,
            function,
            event.is_held,
        ) {
            shortcut_help::KeyAction::Open | shortcut_help::KeyAction::Close => {
                self.toggle_shortcut_help(window, cx);
                cx.stop_propagation();
                return;
            }
            shortcut_help::KeyAction::Scroll(action) => {
                shortcut_help::scroll(self.shortcut_help.scroll_handle(), action);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            shortcut_help::KeyAction::Consume => {
                cx.stop_propagation();
                return;
            }
            shortcut_help::KeyAction::Ignore => {}
        }
        if self.handle_context_menu_key_down(event, window, cx) {
            return;
        }
        if self.tag_input.read(cx).focus_handle().is_focused(window) {
            if !event.is_held
                && handoff::is_browser_copy_shortcut(key, secondary, shift, alt, function)
            {
                self.invalidate_file_handoff();
            }
            let is_composing = self.tag_input.read(cx).is_composing();
            if tag_completion::handle_focused_key_down(
                self,
                key,
                event.is_held,
                is_composing,
                secondary,
                shift,
                alt,
                function,
                cx,
            ) {
                return;
            }
            tags::handle_focused_key_down(
                self,
                key,
                event.is_held,
                is_composing,
                secondary,
                shift,
                alt,
                function,
                window,
                cx,
            );
            return;
        }
        if self
            .tag_filter_input
            .read(cx)
            .focus_handle()
            .is_focused(window)
        {
            if !event.is_held
                && handoff::is_browser_copy_shortcut(key, secondary, shift, alt, function)
            {
                self.invalidate_file_handoff();
            }
            if key == "escape" {
                window.focus(&self.focus_handle);
            }
            return;
        }
        if key.eq_ignore_ascii_case("f") && secondary {
            self.invalidate_file_handoff();
            window.focus(&self.search_input.read(cx).focus_handle());
            return;
        }
        if self.search_input.read(cx).focus_handle().is_focused(window) {
            if !event.is_held
                && handoff::is_browser_copy_shortcut(key, secondary, shift, alt, function)
            {
                self.invalidate_file_handoff();
            }
            if key == "escape" {
                window.focus(&self.focus_handle);
            }
            return;
        }
        if self
            .beat_count_input
            .read(cx)
            .focus_handle()
            .is_focused(window)
        {
            if key == "escape" {
                window.focus(&self.focus_handle);
            }
            return;
        }
        if self.handle_audio_output_key_down(event, window, cx) {
            return;
        }
        if self.audio_settings_open && self.audio_output_focus_handle.is_focused(window) {
            return;
        }
        if self.volume_focus_handle.is_focused(window) {
            if key == "escape" {
                self.cancel_volume_drag(window, cx);
                window.focus(&self.focus_handle);
                cx.notify();
            } else if !event.is_held || volume::is_keyboard_key(key) {
                self.change_volume_from_key(key, shift, cx);
            }
            return;
        }
        if !event.is_held
            && !secondary
            && !shift
            && !alt
            && !function
            && curation::is_shortcut_key(key)
            && self.handle_curation_shortcut(key, cx)
        {
            return;
        }
        if self.waveform_focus_handle.is_focused(window)
            && !secondary
            && !shift
            && !alt
            && !function
            && self.waveform.is_some()
        {
            if key.eq_ignore_ascii_case("z") {
                if self.zoom_waveform_to_selection() {
                    cx.notify();
                }
                return;
            }
            if key.eq_ignore_ascii_case("x") {
                if self.reset_waveform_viewport() {
                    cx.notify();
                }
                return;
            }
            if let Some(direction) =
                waveform::waveform_region_navigation_key(key, secondary, shift, alt, function)
            {
                let canceled = self.cancel_waveform_gestures(window, cx);
                if self.slide_waveform_region(direction) || canceled {
                    cx.notify();
                }
                return;
            }
        }
        if let Some(direction) =
            history::history_shortcut_direction(key, secondary, shift, alt, function, event.is_held)
        {
            self.navigate_playback_history(direction, cx);
            return;
        }
        if handoff::should_route_browser_copy(
            false,
            event.is_held,
            key,
            secondary,
            shift,
            alt,
            function,
        ) {
            self.copy_selected_files(cx);
            return;
        }
        if event.is_held && key == "space" {
            return;
        }
        if secondary && !shift && key.eq_ignore_ascii_case("a") {
            self.select_all_visible(cx);
            return;
        }
        match key {
            "space" => self.toggle_playback(cx),
            "l" | "L" => self.toggle_loop(cx),
            "x" if !shift && !secondary => self.toggle_focused_selection(cx),
            "up" | "arrowup" => self.move_selection(
                -1,
                shift,
                secondary && shift,
                navigation_suppresses_autoplay,
                cx,
            ),
            "down" | "arrowdown" => self.move_selection(
                1,
                shift,
                secondary && shift,
                navigation_suppresses_autoplay,
                cx,
            ),
            "escape" => {
                self.cancel_file_drag_with_window(window, cx);
                self.cancel_waveform_region_drag(window, cx);
                self.cancel_waveform_scrollbar_drag(window, cx);
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
        self.source_registry_task.take();
        if let Some(cancellation) = self.catalog_projection_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.catalog_projection_task.take();
        self.cancel_normalized_audition();
        self.normalized_audition_task.take();
        self.waveform_detail.reset();
        self.reset_catalog_metadata();
        self.cancel_waveform_load();
        self.cancel_audio_output_options();
        self.audio_output_open_generation = self.audio_output_open_generation.wrapping_add(1);
        self.audio_output_open_task.take();
        self.shutdown_file_drag();
        self.shutdown_directory_open();
        self.handoff_generation = self.handoff_generation.wrapping_add(1);
        self.handoff_task.take();
        if let Some(runtime) = self.playback.as_ref() {
            let _ = runtime.handle.try_shutdown();
        }
        if let Some(runtime) = self.audio_output_retiring.as_ref() {
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

fn scan_source_index_for_error(
    sessions: &[SourceSession],
    scan_root: &Path,
    source_index_hint: Option<usize>,
) -> Option<usize> {
    source_index_hint
        .filter(|index| {
            sessions
                .get(*index)
                .is_some_and(|session| session.root == scan_root)
        })
        .or_else(|| {
            sessions
                .iter()
                .position(|session| session.root == scan_root)
        })
}

fn source_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

fn scan_completion_is_current(completion_generation: u64, current_generation: u64) -> bool {
    completion_generation == current_generation
}

fn reveal_should_clear_for_focus_change(
    reveal_id: Option<&AudioFileId>,
    next_id: &AudioFileId,
) -> bool {
    reveal_id.is_some_and(|reveal_id| reveal_id != next_id)
}

fn browser_focus_from_state(
    browser_focus_override: Option<&AudioFileId>,
    browser_focus_detached: bool,
    selected_id: Option<AudioFileId>,
) -> Option<AudioFileId> {
    if let Some(browser_focus_override) = browser_focus_override {
        return Some(browser_focus_override.clone());
    }
    (!browser_focus_detached).then_some(selected_id).flatten()
}

#[cfg(test)]
mod tests {
    use super::{
        SampleClickIntent, SelectionOrigin, SourceSession, ThemeLoadState,
        browser_focus_from_state, resolve_scan_source_index, reveal_should_clear_for_focus_change,
        sample_click_intent, scan_source_index_for_error, selection_origin_for_click,
        selection_origin_for_navigation, should_autoplay_selection,
    };
    use std::path::PathBuf;
    use wavecrate_core::{AudioFile, AudioFileId};
    use wavecrate_library::sample_sources::{SampleSource, SourceId};

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
    fn scan_failure_ignores_stale_index_after_source_reorder() {
        let first = PathBuf::from("/canonical/first");
        let second = PathBuf::from("/canonical/second");
        let sessions = vec![
            SourceSession::new(first),
            SourceSession::new(second.clone()),
        ];

        assert_eq!(
            scan_source_index_for_error(&sessions, &second, Some(0)),
            Some(1)
        );
    }

    #[test]
    fn restored_source_session_starts_with_identity_but_empty_projection() {
        let root = PathBuf::from("/canonical/restored");
        let source = SampleSource::new_with_id(SourceId::from_string("restored-id"), root.clone());
        let session = SourceSession::from_source(&source);

        assert_eq!(session.source_id, Some(source.id));
        assert_eq!(session.root, root);
        assert!(session.entries.is_empty());
        assert_eq!(session.tree, super::FolderTree::default());
        assert!(session.selected_audio_id.is_none());
    }

    #[test]
    fn selection_modifiers_route_autoplay_only_for_plain_focus() {
        let cases = [
            (
                false,
                false,
                false,
                false,
                SampleClickIntent::Focus,
                SelectionOrigin::Intentional,
                true,
            ),
            (
                false,
                false,
                true,
                false,
                SampleClickIntent::Focus,
                SelectionOrigin::Passive,
                false,
            ),
            (
                true,
                false,
                false,
                false,
                SampleClickIntent::ReplaceRange,
                SelectionOrigin::Batch,
                false,
            ),
            (
                false,
                true,
                false,
                false,
                SampleClickIntent::Toggle,
                SelectionOrigin::Batch,
                false,
            ),
            (
                true,
                true,
                false,
                true,
                SampleClickIntent::AddRange,
                SelectionOrigin::Batch,
                false,
            ),
        ];

        for (shift, secondary, alt, function, intent, origin, autoplay) in cases {
            assert_eq!(sample_click_intent(shift, secondary), intent);
            assert_eq!(selection_origin_for_click(intent, alt || function), origin);
            assert_eq!(should_autoplay_selection(true, origin), autoplay);
        }

        for (extend, modifier, expected) in [
            (false, false, SelectionOrigin::Intentional),
            (true, false, SelectionOrigin::Batch),
            (false, true, SelectionOrigin::Batch),
            (true, true, SelectionOrigin::Batch),
        ] {
            assert_eq!(selection_origin_for_navigation(extend, modifier), expected);
            assert_eq!(
                should_autoplay_selection(true, expected),
                !extend && !modifier
            );
        }
    }

    #[test]
    fn focus_change_clears_only_a_reveal_for_another_identity() {
        let first = AudioFile {
            path: PathBuf::from("/source/first.wav"),
            relative_path: PathBuf::from("first.wav"),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        };
        let second = AudioFile {
            path: PathBuf::from("/source/second.wav"),
            relative_path: PathBuf::from("second.wav"),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        };
        let first_id: AudioFileId = first.stable_id();
        let second_id = second.stable_id();
        assert!(!reveal_should_clear_for_focus_change(
            Some(&first_id),
            &first_id
        ));
        assert!(reveal_should_clear_for_focus_change(
            Some(&first_id),
            &second_id
        ));
        assert!(!reveal_should_clear_for_focus_change(None, &second_id));
    }

    #[test]
    fn detached_loaded_focus_stays_hidden_after_a_context_override_is_cleared() {
        let loaded = AudioFile {
            path: PathBuf::from("/source/loaded.wav"),
            relative_path: PathBuf::from("loaded.wav"),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        };
        let context_row = AudioFile {
            path: PathBuf::from("/source/context.wav"),
            relative_path: PathBuf::from("context.wav"),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        };
        let loaded_id = loaded.stable_id();
        let context_id = context_row.stable_id();

        assert_eq!(
            browser_focus_from_state(Some(&context_id), true, Some(loaded_id.clone())),
            Some(context_id)
        );
        assert_eq!(browser_focus_from_state(None, true, Some(loaded_id)), None);
    }
}
