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
    App, AppContext, BorrowAppContext, Bounds, ClickEvent, Context, FocusHandle, Focusable,
    KeyDownEvent, PathPromptOptions, Pixels, ScrollHandle, ScrollStrategy, Task,
    UniformListScrollHandle, Window,
};
use reson::{PlaybackRequestId, PlaybackRuntime};
use yield_core::{
    AudioFile, AudioFileId, BrowserSelection, BrowserViewModel, FolderScope, FolderTree,
    FolderTreeRow, PlaybackLoopPolicy, PlaybackNavigationHistory, RandomNavigationState,
    SortColumn, WaveformEditRegion, WaveformEditSettings, WaveformViewport,
    parse_playmark_frame_range, playmark_selection_label,
};
use yield_design::{Motion, Theme, load_theme_file};
use yield_library::sample_sources::{SampleSource, SourceId, SourceRole};
use yield_media::{
    WaveformPreview, enumerate_audio_files_with_cancel, load_waveform_preview_with_cancel,
};
use yield_session::{CatalogStatus, PreferencePatch, RegistryRevision, RequestId};

use crate::beat_count_input::{BeatCountChanged, BeatCountInput, DEFAULT_BEAT_COUNT};
use crate::search_input::{SearchChanged, SearchInput, SearchInputOptions, SearchInputRejected};
use crate::theme::ActiveTheme;

#[cfg(target_os = "macos")]
mod accessibility;
#[cfg(not(target_os = "macos"))]
mod accessibility {
    use yield_core::SortColumn;

    pub(super) const TAG_FILTER_INPUT: &str = "browser-tag-filter-input";
    pub(super) const TAG_FILTER_TOGGLE: &str = "browser-tag-filter-toggle";
    pub(super) const HARVEST_FILTER_TOGGLE: &str = "browser-harvest-filter-toggle";
    pub(super) const HARVEST_FILTER_SCOPE: &str = "browser-harvest-filter-scope";
    pub(super) const CURATION_QUEUE_TOGGLE: &str = "browser-curation-queue-toggle";
    pub(super) const CURATION_QUEUE_SCOPE: &str = "browser-curation-queue-scope";
    pub(super) const RANDOM_NAVIGATION: &str = "browser-random-navigation";
    pub(super) const NAME_VIEW_MODE: &str = "browser-name-view-mode";
    pub(super) const COLLECTION_RENAME: &str = "browser-collection-rename-input";

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
    pub(super) fn playback_age_filter_id(index: usize) -> String {
        format!("browser-age-filter-{index}")
    }
    pub(super) fn rating_filter_id(index: usize) -> String {
        format!("browser-rating-filter-{index}")
    }
    pub(super) fn harvest_choice_id(index: usize) -> String {
        format!("browser-harvest-filter-choice-{index}")
    }
    pub(super) fn curation_queue_choice_id(index: usize) -> String {
        format!("browser-curation-queue-choice-{index}")
    }
}
mod analysis;
mod audio_output;
mod catalog;
mod catalog_metadata;
mod collection_rename;
mod column_layout;
mod compare_anchor;
mod context_menu;
mod curation;
mod curation_queue;
mod direct_random;
mod directory_open;
mod display_labels;
mod feedback_issue;
mod file_drag;
mod filter_family;
mod filter_panel;
mod folder_keyboard;
mod general_settings;
mod handoff;
mod harvest_filter;
mod history;
mod job_details;
mod metadata_panel;
mod metronome;
mod motion;
mod normalized_audition;
mod os_entry;
mod panel_focus;
mod panel_resize;
mod playback_age;
mod played_ranges;
mod playmark_timing;
mod preferences;
mod random_audition;
mod rating_decay;
mod sample_list_scrollbar;
mod shortcut_help;
mod sidebar;
mod similar_sections;
mod similarity_list;
mod source_recovery;
mod source_registry;
mod status_log;
mod tag_chips;
mod tag_completion;
mod tag_dictionary;
mod tag_filter;
mod tag_library;
mod tag_pager;
mod tags;
mod transient_markers;
mod transport;
mod unsupported_entries;
mod update_check;
mod update_payload;
#[cfg(any(target_os = "macos", test))]
mod user_shader_runtime;
mod view;
mod volume;
mod waveform;
mod waveform_detail;
mod waveform_fades;
mod waveform_scrollbar;
mod zero_crossing;

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
    role: SourceRole,
    root: PathBuf,
    label: String,
    entries: Arc<Vec<AudioFile>>,
    tree: Arc<FolderTree>,
    selected_folder: PathBuf,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    curation_queue_mode: yield_core::CurationQueueMode,
    harvest_mode: harvest_filter::HarvestMode,
    selected_audio_id: Option<AudioFileId>,
    catalog_facts: Arc<HashMap<PathBuf, catalog::CatalogFileFacts>>,
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
            role: SourceRole::Normal,
            root,
            label,
            entries: Arc::new(Vec::new()),
            tree: Arc::new(FolderTree::default()),
            selected_folder: PathBuf::from("."),
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            curation_queue_mode: yield_core::CurationQueueMode::default(),
            harvest_mode: harvest_filter::HarvestMode::default(),
            selected_audio_id: None,
            catalog_facts: Arc::new(HashMap::new()),
            catalog_lifecycle_generation: None,
            last_error: None,
        }
    }

    fn from_source(source: &SampleSource) -> Self {
        let mut session = Self::new(source.root.clone());
        session.source_id = Some(source.id.clone());
        session.role = source.role;
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

#[derive(Clone, Debug, PartialEq)]
struct PlaymarkTimingEditorState {
    source_id: Option<SourceId>,
    source_root: PathBuf,
    path: PathBuf,
    waveform_generation: u64,
    region: waveform::WaveformPlayRegion,
    original_text: String,
    error: Option<String>,
}

#[derive(Clone)]
struct SourceReorderDrag {
    source_id: SourceId,
    revision: RegistryRevision,
    target: Option<(SourceId, bool)>,
}

pub(crate) struct Browser {
    #[cfg(target_os = "macos")]
    accessibility: Option<accessibility::BrowserAccessibility>,
    focus_handle: FocusHandle,
    waveform_focus_handle: FocusHandle,
    volume_focus_handle: FocusHandle,
    search_input: gpui::Entity<SearchInput>,
    _search_subscription: gpui::Subscription,
    playmark_timing_input: gpui::Entity<SearchInput>,
    _playmark_timing_subscription: gpui::Subscription,
    playmark_timing_editor: Option<PlaymarkTimingEditorState>,
    playmark_timing_blur_subscription: Option<gpui::Subscription>,
    collection_rename_input: gpui::Entity<SearchInput>,
    _collection_rename_subscription: gpui::Subscription,
    collection_rename_editor: Option<collection_rename::CollectionRenameEditor>,
    suppress_collection_keyboard_click: Option<usize>,
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
    show_empty_folders: bool,
    selected_folder: PathBuf,
    folder_keyboard: Option<folder_keyboard::FolderKeyboardState>,
    expanded_folders: BTreeSet<PathBuf>,
    folder_scope: FolderScope,
    restore_selection_id: Option<AudioFileId>,
    restore_selection_position: Option<usize>,
    view_model: BrowserViewModel,
    column_layout: column_layout::ColumnLayout,
    column_gesture: Option<column_layout::ColumnGesture>,
    column_header_bounds: Rc<RefCell<Option<gpui::Bounds<gpui::Pixels>>>>,
    suppress_column_sort_click: bool,
    sample_name_view_mode: yield_core::SampleNameViewMode,
    selected: Option<usize>,
    selection: BrowserSelection,
    browser_focus_override: Option<AudioFileId>,
    browser_focus_detached: bool,
    focus_chord_started: Option<(Instant, u64, Option<PathBuf>)>,
    context_menu: Option<context_menu::ContextMenuState>,
    feature_analysis: analysis::FeatureAnalysisProjection,
    waveform: Option<WaveformPreview>,
    waveform_error: Option<String>,
    waveform_render_cache: waveform::WaveformRenderCache,
    waveform_fade_render_cache: waveform_fades::WaveformFadeRenderCache,
    waveform_detail: waveform_detail::WaveformDetailState,
    zero_crossing: zero_crossing::ZeroCrossingState,
    similar_sections: similar_sections::SimilarSectionsState,
    similarity_list: similarity_list::SimilarityListState,
    transient_markers: transient_markers::TransientMarkersState,
    bpm_snap_enabled: bool,
    beat_guides_enabled: bool,
    beat_guide_count: u8,
    random_audition_sticky: bool,
    random_audition_pending: Option<random_audition::PendingRandomAudition>,
    random_navigation: RandomNavigationState,
    random_navigation_scope: Option<(Option<SourceId>, PathBuf)>,
    direct_random_history: direct_random::DirectRandomHistory,
    metronome_enabled: bool,
    playback_metronome_beat_count: Option<u16>,
    playback_metronome_confirmed: bool,
    playback_metronome_grid: Option<metronome::PlaybackMetronomeGrid>,
    playback_metronome_grid_path: Option<PathBuf>,
    normalized_audition: normalized_audition::NormalizedAuditionState,
    waveform_viewport: WaveformViewport,
    preferred_sidebar_width: Option<f32>,
    preferred_filter_height: Option<f32>,
    filter_panel_scroll: ScrollHandle,
    preferred_waveform_height: Option<f32>,
    preferred_metadata_height: Option<f32>,
    preferred_collections_height: Option<f32>,
    pane_resize_drag: Option<view::PaneResizeDrag>,
    pane_resize_activation: Option<gpui::Subscription>,
    pane_resize_environment_subscriptions: Vec<gpui::Subscription>,
    waveform_zoom_anchor_ratio: f64,
    waveform_play_region: Option<waveform::WaveformPlayRegion>,
    waveform_edit_region: Option<WaveformEditRegion>,
    waveform_edit_settings: WaveformEditSettings,
    waveform_fade_hover: Option<waveform_fades::WaveformEditFadeHit>,
    waveform_region_drag: Option<waveform::WaveformRegionDrag>,
    waveform_pan_drag: Option<waveform::WaveformPanDrag>,
    waveform_scrollbar_drag: Option<waveform_scrollbar::WaveformScrollbarDrag>,
    played_ranges: played_ranges::PlayedRanges,
    scan_status: String,
    job_details_open: bool,
    job_details_focus_handle: FocusHandle,
    job_action: Option<job_details::JobActionState>,
    job_action_error: Option<String>,
    status_log: status_log::StatusLog,
    os_entry_status: Option<&'static str>,
    os_entry_task: Option<Task<()>>,
    playback_status: String,
    volume: f32,
    volume_drag: Option<volume::VolumeDrag>,
    volume_runtime_pending: Option<f32>,
    volume_runtime_retry_count: u8,
    volume_status: Option<String>,
    scan_generation: u64,
    playback_age_refresh_generation: u64,
    playback_age_refresh_task: Option<Task<()>>,
    waveform_generation: u64,
    playback_epoch: u64,
    playing_path: Option<PathBuf>,
    compare_anchor: Option<compare_anchor::CompareAnchor>,
    compare_anchor_task: Option<Task<()>>,
    compare_anchor_generation: u64,
    compare_anchor_play_pending: bool,
    playback_policy: PlaybackLoopPolicy,
    accepted_playback_intent: Option<transport::PlaybackIntent>,
    autoplay_selection: bool,
    help_tooltips_enabled: bool,
    playback_history: PlaybackNavigationHistory,
    latest_playback_request_id: Option<PlaybackRequestId>,
    next_playback_retarget_generation: u64,
    pending_loop_retarget: Option<transport::PendingLoopRetarget>,
    pending_progress_poll: Option<transport::PendingProgressPoll>,
    playback_cursor_pending: Option<f32>,
    playback_cursor_sample: Option<transport::PlaybackCursorSample>,
    playback_cursor_interpolation: Option<transport::PlaybackCursorInterpolation>,
    waveform_hover_cursor: Option<f32>,
    waveform_last_play_start: Option<f32>,
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
    source_registry_pending_edit: Option<(RequestId, Option<RegistryRevision>)>,
    source_primary_unconfirmed: bool,
    source_reorder_drag: Option<SourceReorderDrag>,
    source_registry_removed_roots: BTreeSet<PathBuf>,
    source_registry_status: String,
    catalog_next_generation: u64,
    catalog_activation: Option<catalog::CatalogActivation>,
    catalog_pending_command: Option<catalog::CatalogCommandIntent>,
    catalog_pending_removes: HashMap<SourceId, catalog::CatalogCommandIntent>,
    catalog_projection_task: Option<Task<()>>,
    catalog_projection_cancel: Option<Arc<AtomicBool>>,
    catalog_metadata: catalog_metadata::CatalogMetadataState,
    rating_decay: rating_decay::RatingDecayState,
    display_labels: display_labels::DisplayLabelsState,
    source_recovery: source_recovery::SourceRecoveryState,
    curation_state: curation::CurationState,
    latest_curation_task: Option<Task<()>>,
    curation_queue: curation_queue::CurationQueueState,
    harvest_filter: harvest_filter::HarvestRuntime,
    tag_editor: tags::TagEditorState,
    tag_completion: tag_completion::TagCompletionState,
    tag_dictionary: tag_dictionary::TagDictionaryState,
    tag_library: tag_library::TagLibraryState,
    tag_filter: tag_filter::TagFilterState,
    tag_chips: tag_chips::TagChipsState,
    collection_filter_counts: [usize; yield_core::COLLECTION_COUNT],
    preferences: preferences::PreferenceState,
    waveform_cancel: Option<Arc<AtomicBool>>,
    waveform_task: Option<Task<()>>,
    zero_crossing_task: Option<Task<()>>,
    zero_crossing_queued: Option<zero_crossing::ZeroCrossingIntent>,
    normalized_audition_task: Option<Task<()>>,
    normalized_audition_queued: Option<normalized_audition::PendingNormalizedAudition>,
    normalized_audition_deferred_play: Option<normalized_audition::PendingNormalizedAudition>,
    normalized_audition_pending_playback_target: Option<transport::PlaybackRetargetRequest>,
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
    general_settings: general_settings::GeneralSettings,
    audio_settings_open: bool,
    shortcut_help: shortcut_help::ShortcutHelpState,
    unsupported_entries: unsupported_entries::UnsupportedEntriesState,
    feedback_issue: feedback_issue::FeedbackIssueState,
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
    sample_table_horizontal_scroll_handle: ScrollHandle,
    sample_list_scrollbar_drag: Option<sample_list_scrollbar::DragState>,
    sample_list_scrollbar_track_bounds: Option<Bounds<Pixels>>,
    list_scroll_handle: UniformListScrollHandle,
    source_scroll_handle: UniformListScrollHandle,
    folder_scroll_handle: UniformListScrollHandle,
}

impl Browser {
    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_source_ids(&self, _: &Path) -> (Option<String>, Option<String>) {
        (None, None)
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_folder_ids(&self, _: &Path) -> (Option<String>, Option<String>) {
        (None, None)
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_sample_id(&self, _: &Path) -> Option<String> {
        None
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_focus(&self, _: &str) -> Option<FocusHandle> {
        None
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_marker(&self, _: impl Into<String>) -> Option<gpui::AnyElement> {
        None
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn ax_frame_setter(
        &self,
        _: impl Into<String>,
    ) -> Option<Rc<dyn Fn(gpui::Bounds<gpui::Pixels>)>> {
        None
    }

    pub(crate) fn new_window(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let browser = Self::new(cx);
        #[cfg(target_os = "macos")]
        let browser = {
            let mut browser = browser;
            browser.accessibility = accessibility::BrowserAccessibility::new(
                window,
                cx,
                browser.job_details_focus_handle.clone(),
            );
            browser
        };
        #[cfg(not(target_os = "macos"))]
        let _ = window;
        browser
    }

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
                cx.notify();
            });
        let playmark_timing_input =
            cx.new(|cx| SearchInput::new_with_options(cx, SearchInputOptions::playmark_timing()));
        let playmark_timing_subscription = cx.subscribe(
            &playmark_timing_input,
            |view, _input, _: &SearchChanged, cx| {
                if let Some(editor) = view.playmark_timing_editor.as_mut() {
                    editor.error = None;
                    cx.notify();
                }
            },
        );
        let collection_rename_input =
            cx.new(|cx| SearchInput::new_with_options(cx, SearchInputOptions::collection_name()));
        let collection_rename_subscription = cx.subscribe(
            &collection_rename_input,
            |view, _input, _: &SearchChanged, cx| {
                if view.collection_rename_editor.is_some() {
                    cx.notify();
                }
            },
        );
        let beat_count_input = cx.new(BeatCountInput::new);
        let beat_count_subscription = cx.subscribe(
            &beat_count_input,
            |view, _input, event: &BeatCountChanged, cx| {
                view.beat_guide_count = event.count;
                view.apply_metronome_count_change(event.count, cx);
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
            #[cfg(target_os = "macos")]
            accessibility: None,
            focus_handle: cx.focus_handle(),
            waveform_focus_handle: cx.focus_handle(),
            volume_focus_handle: cx.focus_handle(),
            search_input,
            _search_subscription: search_subscription,
            playmark_timing_input,
            _playmark_timing_subscription: playmark_timing_subscription,
            playmark_timing_editor: None,
            playmark_timing_blur_subscription: None,
            collection_rename_input,
            _collection_rename_subscription: collection_rename_subscription,
            collection_rename_editor: None,
            suppress_collection_keyboard_click: None,
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
            show_empty_folders: false,
            selected_folder: PathBuf::from("."),
            folder_keyboard: None,
            expanded_folders: BTreeSet::from([PathBuf::from(".")]),
            folder_scope: FolderScope::DirectOnly,
            restore_selection_id: None,
            restore_selection_position: None,
            view_model: BrowserViewModel::new(&[]),
            column_layout: column_layout::ColumnLayout::default(),
            column_gesture: None,
            column_header_bounds: Rc::new(RefCell::new(None)),
            suppress_column_sort_click: false,
            sample_name_view_mode: yield_core::SampleNameViewMode::default(),
            selected: None,
            selection: BrowserSelection::new(),
            browser_focus_override: None,
            browser_focus_detached: false,
            focus_chord_started: None,
            context_menu: None,
            feature_analysis: analysis::FeatureAnalysisProjection::default(),
            waveform: None,
            waveform_error: None,
            waveform_render_cache: waveform::WaveformRenderCache::default(),
            waveform_fade_render_cache: waveform_fades::WaveformFadeRenderCache::default(),
            waveform_detail: waveform_detail::WaveformDetailState::default(),
            zero_crossing: zero_crossing::ZeroCrossingState::default(),
            similar_sections: similar_sections::SimilarSectionsState::default(),
            similarity_list: similarity_list::SimilarityListState::default(),
            transient_markers: transient_markers::TransientMarkersState::default(),
            bpm_snap_enabled: false,
            beat_guides_enabled: false,
            beat_guide_count: DEFAULT_BEAT_COUNT,
            random_audition_sticky: false,
            random_audition_pending: None,
            random_navigation: RandomNavigationState::default(),
            random_navigation_scope: None,
            direct_random_history: direct_random::DirectRandomHistory::default(),
            metronome_enabled: false,
            playback_metronome_beat_count: None,
            playback_metronome_confirmed: false,
            playback_metronome_grid: None,
            playback_metronome_grid_path: None,
            normalized_audition: normalized_audition::NormalizedAuditionState::default(),
            waveform_viewport: WaveformViewport::full(),
            preferred_sidebar_width: None,
            preferred_filter_height: None,
            filter_panel_scroll: ScrollHandle::new(),
            preferred_waveform_height: None,
            preferred_metadata_height: None,
            preferred_collections_height: None,
            pane_resize_drag: None,
            pane_resize_activation: None,
            pane_resize_environment_subscriptions: Vec::new(),
            waveform_zoom_anchor_ratio: 0.5,
            waveform_play_region: None,
            waveform_edit_region: None,
            waveform_edit_settings: waveform::default_edit_settings(),
            waveform_fade_hover: None,
            waveform_region_drag: None,
            waveform_pan_drag: None,
            waveform_scrollbar_drag: None,
            played_ranges: played_ranges::PlayedRanges::default(),
            scan_status: "Choose a folder or pass one on the command line".to_string(),
            job_details_open: false,
            job_details_focus_handle: cx.focus_handle(),
            job_action: None,
            job_action_error: None,
            status_log: status_log::StatusLog::default(),
            os_entry_status: None,
            os_entry_task: None,
            playback_status: "Ready".to_string(),
            volume: volume::DEFAULT_VOLUME,
            volume_drag: None,
            volume_runtime_pending: None,
            volume_runtime_retry_count: 0,
            volume_status: None,
            scan_generation: 0,
            playback_age_refresh_generation: 0,
            playback_age_refresh_task: None,
            waveform_generation: 0,
            playback_epoch: 0,
            playing_path: None,
            compare_anchor: None,
            compare_anchor_task: None,
            compare_anchor_generation: 0,
            compare_anchor_play_pending: false,
            playback_policy: PlaybackLoopPolicy::default(),
            accepted_playback_intent: None,
            autoplay_selection: true,
            help_tooltips_enabled: true,
            playback_history: PlaybackNavigationHistory::default(),
            latest_playback_request_id: None,
            next_playback_retarget_generation: 0,
            pending_loop_retarget: None,
            pending_progress_poll: None,
            playback_cursor_pending: None,
            playback_cursor_sample: None,
            playback_cursor_interpolation: None,
            waveform_hover_cursor: None,
            waveform_last_play_start: None,
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
            source_registry_pending_edit: None,
            source_primary_unconfirmed: false,
            source_reorder_drag: None,
            source_registry_removed_roots: BTreeSet::new(),
            source_registry_status: "Loading sources…".to_string(),
            catalog_next_generation: 0,
            catalog_activation: None,
            catalog_pending_command: None,
            catalog_pending_removes: HashMap::new(),
            catalog_projection_task: None,
            catalog_projection_cancel: None,
            catalog_metadata: catalog_metadata::CatalogMetadataState::default(),
            rating_decay: rating_decay::RatingDecayState::default(),
            display_labels: display_labels::DisplayLabelsState::default(),
            source_recovery: source_recovery::SourceRecoveryState::default(),
            curation_state: curation::CurationState::default(),
            latest_curation_task: None,
            curation_queue: curation_queue::CurationQueueState::default(),
            harvest_filter: harvest_filter::HarvestRuntime::default(),
            tag_editor: tags::TagEditorState::default(),
            tag_completion: tag_completion::TagCompletionState::default(),
            tag_dictionary: tag_dictionary::TagDictionaryState::default(),
            tag_library: tag_library::TagLibraryState::default(),
            tag_filter: tag_filter::TagFilterState::default(),
            tag_chips: tag_chips::TagChipsState::default(),
            collection_filter_counts: [0; yield_core::COLLECTION_COUNT],
            preferences: preferences::PreferenceState::new(),
            waveform_cancel: None,
            waveform_task: None,
            zero_crossing_task: None,
            zero_crossing_queued: None,
            normalized_audition_task: None,
            normalized_audition_queued: None,
            normalized_audition_deferred_play: None,
            normalized_audition_pending_playback_target: None,
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
            theme_status: "Yield Editorial Terminal".to_string(),
            theme_load: ThemeLoadState::default(),
            theme_task: None,
            theme_settings_open: false,
            general_settings: general_settings::GeneralSettings::default(),
            audio_settings_open: false,
            shortcut_help: shortcut_help::ShortcutHelpState::new(cx),
            unsupported_entries: unsupported_entries::UnsupportedEntriesState::new(cx),
            feedback_issue: feedback_issue::FeedbackIssueState::new(cx),
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
            sample_table_horizontal_scroll_handle: ScrollHandle::new(),
            sample_list_scrollbar_drag: None,
            sample_list_scrollbar_track_bounds: None,
            list_scroll_handle: UniformListScrollHandle::new(),
            source_scroll_handle: UniformListScrollHandle::new(),
            folder_scroll_handle: UniformListScrollHandle::new(),
        };
        browser.start_source_registry_poll(cx);
        browser.start_preferences_poll(cx);
        browser.start_os_entry_consumer(cx);
        browser
    }

    fn selected_id(&self) -> Option<AudioFileId> {
        self.selected
            .and_then(|index| self.entries.get(index))
            .map(AudioFile::stable_id)
    }

    fn active_source_id(&self) -> Option<SourceId> {
        self.active_source
            .and_then(|index| self.source_sessions.get(index))
            .and_then(|session| session.source_id.clone())
    }

    fn selected_audio_path(&self) -> Option<&Path> {
        self.selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.as_path())
    }

    fn playmark_timing_editor_is_current(&self, editor: &PlaymarkTimingEditorState) -> bool {
        self.waveform.is_some()
            && self.waveform_generation == editor.waveform_generation
            && self.waveform_play_region == Some(editor.region)
            && self.selected_audio_path() == Some(editor.path.as_path())
            && self.root.as_deref() == Some(editor.source_root.as_path())
            && self.active_source_id() == editor.source_id
    }

    fn begin_playmark_timing_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(preview) = self.waveform.as_ref() else {
            return false;
        };
        let Some(region) = self.waveform_play_region else {
            return false;
        };
        let Some(source_root) = self.root.clone() else {
            return false;
        };
        let Some(path) = self.selected_audio_path().map(Path::to_path_buf) else {
            return false;
        };
        let Some(core_region) = region.core() else {
            return false;
        };
        let Some(original_text) = playmark_selection_label(
            core_region,
            preview.total_frames,
            preview.sample_rate,
            self.beat_guides_enabled.then_some(self.beat_guide_count),
        ) else {
            return false;
        };
        self.playmark_timing_editor = Some(PlaymarkTimingEditorState {
            source_id: self.active_source_id(),
            source_root,
            path,
            waveform_generation: self.waveform_generation,
            region,
            original_text: original_text.clone(),
            error: None,
        });
        self.playmark_timing_input.update(cx, |input, cx| {
            input.set_content(original_text, cx);
            input.select_all_content(cx);
        });
        let focus_handle = self.playmark_timing_input.read(cx).focus_handle();
        window.focus(&focus_handle);
        cx.notify();
        true
    }

    fn cancel_playmark_timing_edit(&mut self) -> bool {
        self.playmark_timing_editor.take().is_some()
    }

    fn theme_changed_inputs(&self, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.theme_changed(cx));
        self.playmark_timing_input
            .update(cx, |input, cx| input.theme_changed(cx));
        self.collection_rename_input
            .update(cx, |input, cx| input.theme_changed(cx));
    }

    fn commit_playmark_timing_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        restore_focus: bool,
    ) {
        let Some(editor) = self.playmark_timing_editor.clone() else {
            return;
        };
        let draft = self.playmark_timing_input.read(cx).content().to_owned();
        if !self.playmark_timing_editor_is_current(&editor) {
            self.cancel_playmark_timing_edit();
            return;
        }
        if draft == editor.original_text {
            self.cancel_playmark_timing_edit();
            if restore_focus {
                window.focus(&self.waveform_focus_handle);
            }
            cx.notify();
            return;
        }
        let Some(preview) = self.waveform.as_ref() else {
            self.cancel_playmark_timing_edit();
            return;
        };
        let Some(current) = editor.region.core() else {
            self.cancel_playmark_timing_edit();
            return;
        };
        let parsed = parse_playmark_frame_range(
            &draft,
            preview.sample_rate,
            preview.total_frames,
            self.beat_guide_count,
            current,
        );
        let (start_frame, end_frame) = match parsed {
            Ok(range) => range,
            Err(error) => {
                if let Some(editor) = self.playmark_timing_editor.as_mut() {
                    editor.error = Some(error.clone());
                }
                self.set_playback_status(format!("Playmark timing: {error}"));
                let focus_handle = self.playmark_timing_input.read(cx).focus_handle();
                window.focus(&focus_handle);
                cx.notify();
                return;
            }
        };
        if playmark_timing::frame_range_for_region(editor.region, preview.total_frames)
            == Some((start_frame, end_frame))
        {
            self.cancel_playmark_timing_edit();
            if restore_focus {
                window.focus(&self.waveform_focus_handle);
            }
            cx.notify();
            return;
        }
        let total_frames = preview.total_frames as f64;
        let Some(region) = waveform::WaveformPlayRegion::new(
            start_frame as f64 / total_frames,
            end_frame as f64 / total_frames,
        ) else {
            self.cancel_playmark_timing_edit();
            return;
        };
        let path = editor.path.clone();
        self.cancel_playmark_timing_edit();
        self.set_waveform_play_region(Some(region));
        similar_sections::invalidate_if_stale(self);
        match self.submit_playmark_timing_span(&path, region, cx) {
            Some(Ok(())) => {
                self.set_playback_status("Playmark timing applying to playback".to_string());
            }
            Some(Err(error)) => {
                self.set_playback_status(format!("Playmark timing update unavailable: {error}"));
            }
            None => {}
        }
        if restore_focus {
            window.focus(&self.waveform_focus_handle);
        }
        cx.notify();
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
        self.similar_sections.cancel();
        self.random_audition_pending = None;
        self.waveform_detail.cancel();
        zero_crossing::cancel_for_interruption(self);
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
        let visible_ids = self.visible_selection_ids();
        if self.random_navigation.enabled() {
            let focused = self.browser_focus_id();
            self.reconcile_random_navigation_scope(focused.as_ref(), &visible_ids);
            self.random_navigation
                .reconcile(focused.as_ref(), &visible_ids);
        }
        if tag_filter::scope_pending(self) {
            return;
        }
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

    #[cfg(target_os = "macos")]
    fn select_entry_from_accessibility(&mut self, index: usize, cx: &mut Context<Self>) {
        if tag_filter::scope_pending(self) {
            return;
        }
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        self.selection.focus_pointer(&entry.stable_id());
        self.select_entry(
            index,
            Some(motion::FocusTrigger::Keyboard),
            SelectionOrigin::Intentional,
            cx,
        );
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
        self.collection_rename_editor = None;
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
        self.reschedule_playback_age_refresh(cx);
        self.view_model.set_curation_queue(
            &self.entries,
            source_index
                .and_then(|index| self.source_sessions.get(index))
                .map_or(yield_core::CurationQueueMode::default(), |session| {
                    session.curation_queue_mode
                }),
        );
        self.refresh_collection_filter_counts();
        self.selected = None;
        self.cancel_waveform_load();
        self.waveform_pending = false;
        self.waveform = None;
        self.waveform_error = None;
        self.folder_tree = None;
        self.folder_rows_cache.clear();
        self.folder_keyboard = None;
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
        self.set_scan_status(format!("Scanning {}…", root.display()));
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
        scan: yield_media::AudioScan,
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
        session.entries = Arc::new(entries.clone());
        session.tree = Arc::new(tree.clone());
        session.catalog_facts = Arc::new(HashMap::new());
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
        self.reschedule_playback_age_refresh(cx);
        self.view_model.set_curation_queue(
            &self.entries,
            self.source_sessions[source_index].curation_queue_mode,
        );
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        tag_filter::reinstall_after_projection(self, cx);
        curation_queue::reinstall_after_projection(self, cx);
        harvest_filter::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.set_scan_status(format!(
            "{count} audio file{} · {} skipped",
            if count == 1 { "" } else { "s" },
            skipped_entries
        ));
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
            self.folder_keyboard = None;
            self.view_model.set_entries(&self.entries);
            self.reschedule_playback_age_refresh(cx);
            self.view_model
                .set_curation_queue(&self.entries, yield_core::CurationQueueMode::default());
            tag_filter::reinstall_after_projection(self, cx);
            curation_queue::reinstall_after_projection(self, cx);
            harvest_filter::reinstall_after_projection(self, cx);
            self.reset_collection_filter();
            self.set_scan_status(error);
            self.selected = None;
            self.playback_policy.clear_file();
            self.waveform_pending = false;
            self.waveform = None;
            self.waveform_error = None;
            self.invalidate_waveform_generation();
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
        self.entries = self.source_sessions[index].entries.as_ref().clone();
        self.folder_tree = Some(self.source_sessions[index].tree.as_ref().clone());
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
        self.reschedule_playback_age_refresh(cx);
        self.view_model.set_curation_queue(
            &self.entries,
            self.source_sessions[index].curation_queue_mode,
        );
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        tag_filter::reinstall_after_projection(self, cx);
        curation_queue::reinstall_after_projection(self, cx);
        harvest_filter::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.set_scan_status(if is_refresh {
            format!(
                "Could not refresh {}: {error}",
                self.source_sessions[index].label
            )
        } else {
            format!("Could not add source: {error}")
        });
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
            self.set_scan_status("Choose a source folder before refreshing".to_string());
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

    fn refresh_folder_tree_preserving_projection(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.catalog_activation.is_some() && source_recovery::manual_refresh(self, cx) {
            return;
        }
        if self
            .source_sessions
            .get(index)
            .is_some_and(|session| session.source_id.is_some())
            && matches!(
                catalog::catalog_status(cx),
                Some(CatalogStatus::Loading | CatalogStatus::Ready)
            )
            && self.begin_catalog_activation(index, true, true, false, cx)
        {
            return;
        }
        if self.catalog_activation.is_some() {
            self.deactivate_catalog_source(cx);
        }
        let Some(root) = self
            .source_sessions
            .get(index)
            .map(|session| session.root.clone())
        else {
            return;
        };
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.scan_task.take();
        self.restore_selection_id = self.selected_id();
        self.restore_selection_position = self.selected_visible_position();
        self.scan_generation = self.scan_generation.wrapping_add(1);
        let generation = self.scan_generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancellation));
        self.scan_status = format!("Scanning {}…", root.display());
        cx.notify();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let scan_root = root.clone();
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
                    Ok(scan) => view.apply_scan_result(scan, Some(index), cx),
                    Err(error) => view.apply_scan_error(Some(index), scan_root, error, cx),
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn toggle_folder_scope(&mut self, cx: &mut Context<Self>) {
        self.invalidate_file_handoff();
        self.folder_scope = self.folder_scope.toggled();
        self.update_active_folder_projection(cx);
    }

    pub(super) fn toggle_empty_folder_visibility(&mut self, cx: &mut Context<Self>) {
        self.show_empty_folders = !self.show_empty_folders;
        self.context_menu = None;
        self.invalidate_file_handoff();
        if !self.show_empty_folders {
            self.reset_focus();
        }
        let previous_folder = self.selected_folder.clone();
        self.rebuild_folder_rows_cache();
        if self.selected_folder != previous_folder {
            self.selection.clear();
            self.update_active_folder_projection(cx);
        } else {
            cx.notify();
        }
        if self.show_empty_folders
            && let Some(index) = self.active_source
        {
            self.refresh_folder_tree_preserving_projection(index, cx);
        }
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
        if !self.folder_rows_cache.iter().any(|row| row.path == path) {
            return;
        }
        window.focus(&self.focus_handle);
        self.invalidate_file_handoff();
        self.selection.clear();
        self.selected_folder = path.clone();
        if let Some(root) = self.root.as_ref() {
            if self
                .folder_keyboard
                .as_ref()
                .is_some_and(|state| state.root() != root)
            {
                self.folder_keyboard = None;
            }
            self.folder_keyboard
                .get_or_insert_with(|| {
                    folder_keyboard::FolderKeyboardState::new(root.clone(), path.clone())
                })
                .pointer_select(path.clone());
        }
        self.update_active_folder_projection(cx);
        self.start_focus(
            motion::FocusTarget::Folder(path.clone()),
            motion::FocusTrigger::Pointer,
        );
        self.focus_folder_path(path, window, cx);
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
        if let Some(tree) = self.folder_tree.as_ref() {
            let visible_folder =
                tree.nearest_visible_ancestor(&self.selected_folder, self.show_empty_folders);
            if visible_folder != self.selected_folder {
                self.selected_folder = visible_folder.clone();
                if let Some(index) = self.active_source
                    && let Some(session) = self.source_sessions.get_mut(index)
                {
                    session.selected_folder = visible_folder;
                    session.selected_audio_id = None;
                }
            }
        }
        self.folder_rows_cache = self
            .folder_tree
            .as_ref()
            .map(|tree| tree.visible_rows(&self.expanded_folders, self.show_empty_folders))
            .unwrap_or_default();
        self.ensure_folder_keyboard();
    }

    fn select_entry(
        &mut self,
        index: usize,
        trigger: Option<motion::FocusTrigger>,
        origin: SelectionOrigin,
        cx: &mut Context<Self>,
    ) {
        self.random_audition_pending = None;
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
        self.set_playback_status("Ready".to_string());
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
                    Ok(preview) => {
                        view.waveform = Some(preview);
                        transient_markers::schedule(view, cx);
                    }
                    Err(error) => {
                        view.clear_waveform_region();
                        view.waveform_error = Some(error);
                    }
                }
                view.finish_pending_random_audition(cx);
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
        self.prompt_for_source(cx);
    }

    fn prompt_for_source(&mut self, cx: &mut Context<Self>) {
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
                    view.set_scan_status(format!("Could not choose folder: {error}"));
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
    }

    fn choose_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.choose_theme_from_accessibility(cx);
    }

    fn choose_theme_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a Yield theme JSON file".into()),
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
                    view.set_theme_status(format!("Theme picker error: {error}"));
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_theme_settings_from_accessibility(window, cx);
    }

    fn toggle_theme_settings_from_accessibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_settings_open {
            self.close_general_settings(window, cx);
        } else {
            if self.general_settings.previous_focus.is_none() {
                self.general_settings.previous_focus = window.focused(cx);
            }
            self.theme_settings_open = true;
            self.audio_settings_open = false;
            self.audio_output_dropdown = None;
            self.cancel_audio_output_options();
            self.refresh_global_storage(cx);
            let first = if self.preferences_editable() {
                "settings-theme-choose"
            } else {
                "settings-theme-reset"
            };
            let focus = self
                .ax_focus(first)
                .unwrap_or_else(|| self.focus_handle.clone());
            window.focus(&focus);
            cx.notify();
        }
    }

    pub(super) fn toggle_motion(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_motion_from_accessibility(cx);
    }

    fn toggle_motion_from_accessibility(&mut self, cx: &mut Context<Self>) {
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
        self.theme_changed_inputs(cx);
        cx.notify();
    }

    fn hover_help_visible(&self) -> bool {
        self.help_tooltips_enabled && !self.shortcut_help.is_open()
    }

    fn toggle_help_tooltips(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_help_tooltips_from_accessibility(cx);
    }

    fn toggle_help_tooltips_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let enabled = !self.help_tooltips_enabled;
        if !self.submit_preference_patch(PreferencePatch::SetHelpTooltipsEnabled(enabled), cx) {
            return;
        }
        self.help_tooltips_enabled = enabled;
        cx.notify();
    }

    fn toggle_autoplay(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_autoplay_from_accessibility(cx);
    }

    fn toggle_autoplay_from_accessibility(&mut self, cx: &mut Context<Self>) {
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
        self.set_playback_status(if self.autoplay_selection {
            "Autoplay enabled".to_string()
        } else {
            "Autoplay disabled".to_string()
        });
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
        self.toggle_bpm_snap(cx);
    }

    fn toggle_bpm_snap(&mut self, cx: &mut Context<Self>) {
        self.bpm_snap_enabled = !self.bpm_snap_enabled;
        cx.notify();
    }

    fn toggle_zero_crossing_from_toolbar(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        zero_crossing::toggle_from_toolbar(self, event, window, cx);
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

    fn toggle_transient_markers_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        transient_markers::toggle(self, cx);
    }

    fn random_audition_available(&self) -> bool {
        self.waveform.is_some() || self.selected.is_some() || !self.entries.is_empty()
    }

    fn random_audition_from_toolbar(
        &mut self,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.random_audition_available() {
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.platform || (!cfg!(target_os = "macos") && modifiers.control) {
            self.toggle_sticky_random_audition(cx);
        } else {
            self.request_random_audition(modifiers.shift, cx);
        }
    }

    fn toggle_metronome_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_metronome(cx);
    }

    fn reload_theme(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.reload_theme_from_accessibility(cx);
    }

    fn reload_theme_from_accessibility(&mut self, cx: &mut Context<Self>) {
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
            self.set_theme_status("Choose a theme file before reloading".to_string());
            cx.notify();
        }
    }

    fn reset_theme(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_theme_from_accessibility(window, cx);
    }

    fn reset_theme_from_accessibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.reset_preferences(window, cx);
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
        self.set_theme_status("Yield Editorial Terminal".to_string());
        self.theme_changed_inputs(cx);
        cx.notify();
    }

    fn reset_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.disable_user_shader_locally(window, "Custom shader disabled while settings reset.");
        self.general_settings.visual_effect_local_enabled = Some(false);
        if !self.submit_preference_patch(PreferencePatch::Reset, cx) {
            self.general_settings
                .mark_local_visual_effect_disable_unpersisted("settings unavailable");
            cx.notify();
            return;
        }
        self.general_settings
            .visual_effect_persistence_status
            .clear();
        self.theme_load.invalidate();
        self.theme_task.take();
        cx.set_global(ActiveTheme::default());
        self.set_theme_status("Yield Editorial Terminal".to_string());
        self.autoplay_selection = true;
        self.help_tooltips_enabled = true;
        self.general_settings.rating_decay_weeks = yield_session::DEFAULT_RATING_DECAY_WEEKS;
        self.general_settings.trash_folder = None;
        self.general_settings.invalidate_picker();
        let previous_loop_mode = self.playback_policy.enabled();
        self.playback_policy.reset(false);
        let loop_mode_changed = previous_loop_mode != self.playback_policy.enabled();
        self.apply_preference_volume(volume::DEFAULT_VOLUME);
        if loop_mode_changed {
            self.apply_loop_mode_change(self.playback_policy.enabled(), cx);
        }
        self.theme_changed_inputs(cx);
        cx.notify();
    }

    fn load_theme(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let generation = self.theme_load.begin(path.clone());
        self.theme_task.take();
        self.set_theme_status(format!("Loading {}…", path.display()));
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
                            view.set_theme_status(format!("Theme rejected: {error}"));
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
        self.set_theme_status(theme_name);
        self.theme_changed_inputs(cx);
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
            if self.random_navigation.enabled() {
                let focused = self.browser_focus_id();
                self.reconcile_random_navigation_scope(focused.as_ref(), &[]);
                self.random_navigation.reconcile(focused.as_ref(), &[]);
            }
            return;
        }
        let focused_id = self.browser_focus_id();
        let random_enabled = self.random_navigation.enabled();
        let next_row = if random_enabled {
            let visible_ids = self.visible_selection_ids();
            self.reconcile_random_navigation_scope(focused_id.as_ref(), &visible_ids);
            let target = if direction < 0 {
                self.random_navigation
                    .previous(focused_id.as_ref(), &visible_ids)
            } else {
                self.random_navigation.next(
                    focused_id.as_ref(),
                    &visible_ids,
                    rand::random::<f32>(),
                )
            };
            let Some(position) = target
                .as_ref()
                .and_then(|id| self.view_model.visible_position_for(id))
            else {
                return;
            };
            self.view_model.rows()[position].clone()
        } else {
            let current = focused_id
                .as_ref()
                .and_then(|id| self.view_model.visible_position_for(id))
                .unwrap_or(0) as isize;
            let next_position =
                (current + direction).clamp(0, self.view_model.rows().len() as isize - 1) as usize;
            self.view_model.rows()[next_position].clone()
        };
        self.invalidate_file_handoff();
        let next = next_row.entry_index;
        let next_id = next_row.id;
        let extend = extend && !random_enabled;
        if random_enabled {
            self.selection.focus_pointer(&next_id);
        } else if extend {
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

    fn play_direct_random_sample(&mut self, previous: bool, cx: &mut Context<Self>) {
        if previous {
            let Some(visit) = self.direct_random_history.previous() else {
                self.set_playback_status("No earlier random sample");
                cx.notify();
                return;
            };
            self.replay_direct_random_visit(visit, cx);
            return;
        }
        let visible_ids = self
            .view_model
            .rows()
            .iter()
            .filter_map(|row| {
                self.entries.get(row.entry_index).and_then(|entry| {
                    yield_library::sample_sources::is_supported_audio(&entry.path)
                        .then_some(row.id.clone())
                })
            })
            .collect::<Vec<_>>();
        let scope = self.current_random_navigation_scope();
        let scope = scope.filter(|(_, root)| !self.source_registry_removed_roots.contains(root));
        let focused = self.browser_focus_id();
        let target = self.direct_random_history.next(
            scope,
            &visible_ids,
            focused.as_ref(),
            rand::random::<f32>(),
        );
        let Some(target) = target else {
            self.set_playback_status("No visible WAV sample for random playback");
            cx.notify();
            return;
        };
        let Some(index) = self
            .view_model
            .visible_position_for(&target)
            .and_then(|position| self.view_model.rows().get(position))
            .map(|row| row.entry_index)
        else {
            return;
        };
        self.selection.focus_pointer(&target);
        self.select_entry(
            index,
            Some(motion::FocusTrigger::Keyboard),
            SelectionOrigin::Passive,
            cx,
        );
        self.start_playback_from_position(0.0, cx);
    }

    fn current_random_navigation_scope(&self) -> Option<(Option<SourceId>, PathBuf)> {
        self.active_source
            .and_then(|index| self.source_sessions.get(index))
            .map(|session| (session.source_id.clone(), session.root.clone()))
            .or_else(|| self.root.as_ref().map(|root| (None, root.clone())))
    }

    fn reconcile_random_navigation_scope(
        &mut self,
        focused: Option<&AudioFileId>,
        visible_ids: &[AudioFileId],
    ) {
        let scope = self.current_random_navigation_scope();
        if self.random_navigation_scope != scope {
            self.random_navigation.reset(focused, visible_ids);
            self.random_navigation_scope = scope;
        }
    }

    fn toggle_random_navigation(&mut self, cx: &mut Context<Self>) {
        let enabled = !self.random_navigation.enabled();
        let visible_ids = self.visible_selection_ids();
        let focused = self.browser_focus_id();
        self.random_navigation
            .set_enabled(enabled, focused.as_ref(), &visible_ids);
        self.random_navigation_scope = self.current_random_navigation_scope();
        cx.notify();
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
        if self.handle_unsupported_key(event, window, cx) {
            return;
        }
        self.suppress_collection_keyboard_click = None;
        if key == "escape" && self.source_reorder_drag.take().is_some() {
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if key == "escape" && self.pane_resize_drag.is_some() {
            self.cancel_pane_resize(cx);
            cx.stop_propagation();
            return;
        }
        if self.handle_feedback_key(event, window, cx) {
            return;
        }
        if self
            .collection_rename_input
            .read(cx)
            .focus_handle()
            .is_focused(window)
        {
            if !self
                .collection_rename_editor
                .as_ref()
                .is_some_and(|editor| self.collection_rename_is_current(editor.slot))
            {
                self.collection_rename_editor = None;
                window.focus(&self.focus_handle);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            self.focus_chord_started = None;
            if key == "escape" {
                self.cancel_collection_rename(window, cx);
                cx.stop_propagation();
            } else if key == "enter" {
                if !event.is_held && !self.collection_rename_input.read(cx).is_composing() {
                    self.commit_collection_rename(window, cx);
                }
                cx.stop_propagation();
            }
            return;
        }
        let secondary = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
        let alt = event.keystroke.modifiers.alt;
        let function = event.keystroke.modifiers.function;
        let navigation_suppresses_autoplay = secondary || alt || function;
        let shift = event.keystroke.modifiers.shift;
        if self.job_details_open
            && self.job_details_focus_handle.is_focused(window)
            && key == "escape"
            && !event.is_held
        {
            self.job_details_open = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.shortcut_help.is_open()
            || self.theme_settings_open
            || self.audio_settings_open
            || self.context_menu.is_some()
            || secondary
            || shift
            || alt
            || function
        {
            self.focus_chord_started = None;
        }
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
        if status_log::is_copy_shortcut(
            key,
            event.keystroke.modifiers.platform,
            shift,
            alt,
            function,
            event.is_held,
        ) {
            self.copy_status_log(cx);
            cx.stop_propagation();
            return;
        }
        if self.theme_settings_open && key == "escape" {
            self.close_general_settings(window, cx);
            cx.stop_propagation();
            return;
        }
        #[cfg(target_os = "macos")]
        if !secondary
            && !alt
            && !function
            && self.handle_settings_accessibility_key(key, shift, window, cx)
        {
            cx.stop_propagation();
            return;
        }
        if self.handle_context_menu_key_down(event, window, cx) {
            return;
        }
        if curation_queue::handle_key_down(
            self,
            key,
            event.is_held,
            secondary || alt || shift || function,
            window,
            cx,
        ) {
            cx.stop_propagation();
            return;
        }
        if harvest_filter::handle_key_down(
            self,
            key,
            event.is_held,
            secondary || alt || shift || function,
            window,
            cx,
        ) {
            cx.stop_propagation();
            return;
        }
        if self.tag_input.read(cx).focus_handle().is_focused(window)
            || self
                .tag_filter_input
                .read(cx)
                .focus_handle()
                .is_focused(window)
            || self
                .playmark_timing_input
                .read(cx)
                .focus_handle()
                .is_focused(window)
            || self.search_input.read(cx).focus_handle().is_focused(window)
            || self
                .beat_count_input
                .read(cx)
                .focus_handle()
                .is_focused(window)
        {
            self.focus_chord_started = None;
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
        if self
            .playmark_timing_input
            .read(cx)
            .focus_handle()
            .is_focused(window)
        {
            if key == "escape" {
                self.cancel_playmark_timing_edit();
                window.focus(&self.waveform_focus_handle);
                cx.notify();
                cx.stop_propagation();
            } else if key == "enter" {
                if !event.is_held && !self.playmark_timing_input.read(cx).is_composing() {
                    self.commit_playmark_timing_edit(window, cx, true);
                }
                cx.stop_propagation();
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
        if self.handle_panel_focus_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.handle_folder_key(event, window, cx) {
            cx.stop_propagation();
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
        if !secondary && !shift && !alt && !function && key.eq_ignore_ascii_case("x") {
            if !event.is_held
                && !self.theme_settings_open
                && !self.audio_settings_open
                && self.context_menu.is_none()
            {
                if self.waveform.is_some()
                    && !self
                        .waveform_viewport
                        .is_effectively_equal(WaveformViewport::full())
                {
                    if self.reset_waveform_viewport() {
                        cx.notify();
                    }
                } else {
                    self.toggle_focused_selection(cx);
                }
            }
            return;
        }
        if self.waveform_focus_handle.is_focused(window)
            && !secondary
            && !function
            && self.waveform.is_some()
        {
            if !shift && !alt && key.eq_ignore_ascii_case("i") {
                if !event.is_held {
                    transient_markers::toggle(self, cx);
                }
                return;
            }
            if !shift && !alt && key.eq_ignore_ascii_case("b") {
                if !event.is_held {
                    self.toggle_bpm_snap(cx);
                }
                return;
            }
            if !shift && !alt && key.eq_ignore_ascii_case("z") {
                if self.zoom_waveform_to_selection() {
                    cx.notify();
                }
                return;
            }
            if let Some(direction) =
                waveform::waveform_region_navigation_key(key, secondary, shift, alt, function)
            {
                let canceled = self.cancel_waveform_gestures(window, cx);
                let moved = self.slide_waveform_region(direction);
                if moved {
                    if direction.is_fine() {
                        if let (Some(path), Some(region)) = (
                            self.selected
                                .and_then(|index| self.entries.get(index))
                                .map(|entry| entry.path.clone()),
                            self.waveform_play_region,
                        ) {
                            match self.submit_playmark_timing_span(&path, region, cx) {
                                Some(Ok(())) => {
                                    self.playback_status =
                                        "Fine selection nudge applying to playback".to_string();
                                }
                                Some(Err(error)) => {
                                    self.playback_status =
                                        format!("Fine selection nudge unavailable: {error}");
                                }
                                None => {}
                            }
                        }
                    } else {
                        zero_crossing::schedule_for_committed_region(self, cx);
                    }
                }
                if moved || canceled {
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
        if let Some(action) = direct_random_shortcut(
            key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            shift,
            alt,
            function,
            event.is_held,
        ) {
            self.play_direct_random_sample(action == DirectRandomShortcut::Previous, cx);
            return;
        }
        if random_section_shortcut(
            key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            shift,
            alt,
            function,
            event.is_held,
        ) {
            if self.random_audition_available() {
                self.request_random_audition(false, cx);
            }
            return;
        }
        if secondary && !shift && key.eq_ignore_ascii_case("a") {
            self.select_all_visible(cx);
            return;
        }
        if !secondary && !alt && !function && shift && key == "space" {
            self.play_from_current_play_start(cx);
            return;
        }
        if current_playhead_shortcut(
            key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            shift,
            alt,
            function,
            event.is_held,
        ) {
            self.play_from_current_playhead(cx);
            return;
        }
        if !secondary && !alt && !function && !shift && key.eq_ignore_ascii_case("c") {
            self.set_compare_anchor_from_focus(cx);
            return;
        }
        if !secondary && !alt && !function && shift && key.eq_ignore_ascii_case("c") {
            self.play_compare_anchor(cx);
            return;
        }
        match key {
            "space" if !secondary && !alt && !function && !shift && self.random_audition_sticky => {
                self.request_random_audition(false, cx)
            }
            "space" if !secondary && !alt && !function && !shift => self.play_selected_sample(cx),
            "space" => return,
            "l" | "L" if shift && !secondary && !alt && !function && !event.is_held => {
                self.toggle_loop_lock(cx)
            }
            "l" | "L" if !shift && !secondary && !alt && !function && !event.is_held => {
                self.toggle_loop(cx)
            }
            "up" | "arrowup" | "down" | "arrowdown" => {
                let follow_row = self.browser_surface_focused(window)
                    && !self.theme_settings_open
                    && !self.audio_settings_open
                    && !self.shortcut_help.is_open()
                    && self.context_menu.is_none();
                let direction = if matches!(key, "up" | "arrowup") {
                    -1
                } else {
                    1
                };
                self.move_selection(
                    direction,
                    shift,
                    secondary && shift,
                    navigation_suppresses_autoplay,
                    cx,
                );
                if follow_row && let Some(id) = self.browser_focus_id() {
                    self.focus_keyboard_navigation_row(id, window, cx);
                }
            }
            "escape" => {
                self.cancel_column_gesture(cx);
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
        self.latest_curation_task.take();
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.source_registry_task.take();
        self.os_entry_task.take();
        if let Some(cancellation) = self.catalog_projection_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.catalog_projection_task.take();
        self.cancel_normalized_audition();
        self.normalized_audition_task.take();
        self.waveform_detail.reset();
        self.reset_catalog_metadata();
        self.cancel_waveform_load();
        zero_crossing::cancel_for_shutdown(self);
        self.zero_crossing_task.take();
        self.cancel_audio_output_options();
        self.audio_output_open_generation = self.audio_output_open_generation.wrapping_add(1);
        self.audio_output_open_task.take();
        self.shutdown_file_drag();
        self.shutdown_directory_open();
        self.handoff_generation = self.handoff_generation.wrapping_add(1);
        self.handoff_task.take();
        self.compare_anchor_task.take();
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

fn current_playhead_shortcut(
    key: &str,
    command: bool,
    control: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> bool {
    key == "space" && command && !control && !shift && !alt && !function && !held
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectRandomShortcut {
    Next,
    Previous,
}

fn direct_random_shortcut(
    key: &str,
    command: bool,
    control: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> Option<DirectRandomShortcut> {
    if !key.eq_ignore_ascii_case("r") || !shift || control || alt || function || held {
        return None;
    }
    Some(if command {
        DirectRandomShortcut::Previous
    } else {
        DirectRandomShortcut::Next
    })
}

fn random_section_shortcut(
    key: &str,
    command: bool,
    control: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> bool {
    key.eq_ignore_ascii_case("r") && !command && !control && !shift && !alt && !function && !held
}

#[cfg(test)]
mod tests {
    use super::{
        DirectRandomShortcut, SampleClickIntent, SelectionOrigin, SourceSession, ThemeLoadState,
        browser_focus_from_state, current_playhead_shortcut, direct_random_shortcut,
        random_section_shortcut, resolve_scan_source_index, reveal_should_clear_for_focus_change,
        sample_click_intent, scan_source_index_for_error, selection_origin_for_click,
        selection_origin_for_navigation, should_autoplay_selection,
    };
    use std::path::PathBuf;
    use yield_core::{AudioFile, AudioFileId};
    use yield_library::sample_sources::{SampleSource, SourceId};

    #[test]
    fn current_playhead_shortcut_requires_unmodified_command_space() {
        assert!(current_playhead_shortcut(
            "space", true, false, false, false, false, false
        ));
        assert!(!current_playhead_shortcut(
            "space", false, false, false, false, false, false
        ));
        assert!(!current_playhead_shortcut(
            "space", true, true, false, false, false, false
        ));
        assert!(!current_playhead_shortcut(
            "space", true, false, true, false, false, false
        ));
        assert!(!current_playhead_shortcut(
            "space", true, false, false, true, false, false
        ));
        assert!(!current_playhead_shortcut(
            "space", true, false, false, false, true, false
        ));
        assert!(!current_playhead_shortcut(
            "space", true, false, false, false, false, true
        ));
    }

    #[test]
    fn direct_random_shortcut_requires_shift_r_without_other_modifiers() {
        assert_eq!(
            direct_random_shortcut("r", false, false, true, false, false, false),
            Some(DirectRandomShortcut::Next)
        );
        assert_eq!(
            direct_random_shortcut("R", true, false, true, false, false, false),
            Some(DirectRandomShortcut::Previous)
        );
        assert_eq!(
            direct_random_shortcut("r", false, false, false, false, false, false),
            None
        );
        assert_eq!(
            direct_random_shortcut("r", false, true, true, false, false, false),
            None
        );
        assert_eq!(
            direct_random_shortcut("r", false, false, true, true, false, false),
            None
        );
        assert_eq!(
            direct_random_shortcut("r", false, false, true, false, false, true),
            None
        );
    }

    #[test]
    fn random_section_shortcut_accepts_only_plain_r() {
        assert!(random_section_shortcut(
            "r", false, false, false, false, false, false
        ));
        for (key, command, control, shift, alt, function, held) in [
            ("space", false, false, false, false, false, false),
            ("space", false, true, false, false, false, false),
            ("space", false, false, false, true, false, false),
            ("space", false, true, false, true, false, false),
            ("space", true, true, false, false, false, false),
            ("space", false, true, true, false, false, false),
            ("space", false, false, false, true, true, false),
            ("space", false, false, false, true, false, true),
            ("r", false, true, false, false, false, false),
            ("r", true, false, false, false, false, false),
            ("r", false, false, true, false, false, false),
            ("r", false, false, false, true, false, false),
            ("r", false, false, false, false, true, false),
            ("r", false, false, false, false, false, true),
        ] {
            assert!(!random_section_shortcut(
                key, command, control, shift, alt, function, held
            ));
        }
    }

    #[gpui::test]
    fn direct_random_action_selects_a_different_visible_wav_and_requests_its_playback(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(super::Browser::new);
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/direct-random-fixture");
            let entries = ["a.wav", "b.wav"]
                .into_iter()
                .map(|name| AudioFile {
                    path: root.join(name),
                    relative_path: PathBuf::from(name),
                    byte_len: 0,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                })
                .collect::<Vec<_>>();
            let mut source = SourceSession::new(root.clone());
            source.entries = entries.clone().into();
            view.source_sessions = vec![source];
            view.active_source = Some(0);
            view.root = Some(root);
            view.entries = entries;
            view.view_model.set_entries(&view.entries);
            view.selected = Some(0);
            assert_eq!(view.view_model.rows().len(), 2);
            view.play_direct_random_sample(false, cx);
            assert_eq!(view.selected, Some(1));
            assert_eq!(
                view.playback_init_intent
                    .as_ref()
                    .map(|intent| intent.path.as_path()),
                Some(view.entries[1].path.as_path())
            );
        });
    }

    #[gpui::test]
    fn explicit_actions_cancel_a_pending_random_load(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(super::Browser::new);
        browser.update(cx, |view, cx| {
            let path = PathBuf::from("/tmp/inert-random.wav");
            view.entries = vec![AudioFile {
                path: path.clone(),
                relative_path: PathBuf::from("inert-random.wav"),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            let pending = || super::random_audition::PendingRandomAudition {
                generation: 0,
                path: path.clone(),
                units: yield_core::RandomAuditionUnits::new(0.25, 0.5),
            };
            view.random_audition_pending = Some(pending());
            view.select_entry(0, None, SelectionOrigin::Passive, cx);
            assert!(view.random_audition_pending.is_none());
            view.random_audition_pending = Some(pending());
            view.stop_playback();
            assert!(view.random_audition_pending.is_none());
            view.random_audition_pending = Some(pending());
            view.request_playback_intent(
                super::transport::PlaybackIntent {
                    start: f64::NAN,
                    region: None,
                    looped: false,
                    volume: 1.0,
                    path: path.clone(),
                    origin: super::transport::PlaybackIntentOrigin::Explicit,
                    resume_desired: None,
                },
                cx,
            );
            assert!(view.random_audition_pending.is_none());
            view.random_audition_pending = Some(pending());
            view.cancel_waveform_load();
            assert!(view.random_audition_pending.is_none());
        });
    }

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
        assert_eq!(*session.tree, super::FolderTree::default());
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
