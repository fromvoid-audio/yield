#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded native-style tag library panel.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::TryRecvError;

use gpui::{ClickEvent, Context, Entity, ScrollHandle, Window, div, prelude::*, px};
use yield_core::{TAG_CATEGORIES, TagCategory, infer_tag_category};
use yield_design::Theme;
use yield_library::sample_sources::{
    PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES, PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE,
    ProfileTagCategory, ProfileTagVocabularyLookup, ProfileTagVocabularyPage,
    ProfileTagVocabularyPageCursor, ProfileTagVocabularySetReceipt, SourceTagPageCursor,
    SourceTagPageEntry,
};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogSubmitError, CatalogTagPageQuery, VocabularyRequest,
    VocabularySubmitError,
};

use crate::theme::color;

use super::catalog;
use super::curation::TagAdmissionResult;
use super::source_registry::SourceRegistryGlobal;
use super::tag_pager::{
    CandidateToken, MergedCandidate, ProfilePageWindow, StreamCheckpoint, TagPagerState,
    merge_ready_page, push_page_history,
};
use super::{Browser, tags};

const PANEL_PAGE_SIZE: usize = 64;
const MAX_CATEGORY_LOOKUP_LABELS: usize = 64;
const CATEGORY_COUNT: usize = 5;

/// Browser-owned state for the optional Tag Editor panel.
pub(super) struct TagLibraryState {
    open: bool,
    expanded: [bool; CATEGORY_COUNT],
    generation: u64,
    dirty: bool,
    target: Option<PanelTarget>,
    source_pending: Option<PendingSourcePage>,
    source_target: Option<SourcePageTarget>,
    source_ready: bool,
    profile_pending: Option<PendingProfilePage>,
    profile_target: Option<ProfilePageTarget>,
    profile_ready: bool,
    page_materialized: bool,
    counts_requested: bool,
    categories_requested: bool,
    pager: TagPagerState,
    counts_pending: Option<PendingCountsPage>,
    counts_target: Option<CountsTarget>,
    assignment_counts: BTreeMap<i64, u64>,
    profile_lookup_pending: Option<PendingProfileLookup>,
    profile_lookup_target: Option<ProfileLookupTarget>,
    profile_lookup_state: ProfileLookupState,
    profile_categories: BTreeMap<String, ProfileTagCategory>,
    profile_revision: Option<u64>,
    metadata_retry_ticks: u8,
    category_pending: Option<PendingCategoryWrite>,
    dragged: Option<DragTarget>,
    message: Option<String>,
    scroll_handle: ScrollHandle,
}

impl Default for TagLibraryState {
    fn default() -> Self {
        Self {
            open: false,
            expanded: [true; CATEGORY_COUNT],
            generation: 0,
            dirty: false,
            target: None,
            source_pending: None,
            source_target: None,
            source_ready: false,
            profile_pending: None,
            profile_target: None,
            profile_ready: false,
            page_materialized: false,
            counts_requested: false,
            categories_requested: false,
            pager: TagPagerState::default(),
            counts_pending: None,
            counts_target: None,
            assignment_counts: BTreeMap::new(),
            profile_lookup_pending: None,
            profile_lookup_target: None,
            profile_lookup_state: ProfileLookupState::NotRequested,
            profile_categories: BTreeMap::new(),
            profile_revision: None,
            metadata_retry_ticks: 0,
            category_pending: None,
            dragged: None,
            message: None,
            scroll_handle: ScrollHandle::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PanelTarget {
    generation: u64,
    context: tags::TagSubmissionContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourcePageTarget {
    generation: u64,
    context: tags::TagSubmissionContext,
    after_cursor: Option<SourceTagPageCursor>,
    skip: usize,
}

struct PendingSourcePage {
    request_id: u64,
    target: SourcePageTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProfilePageTarget {
    generation: u64,
    context: tags::TagSubmissionContext,
    expected_revision: Option<u64>,
    after_cursor: Option<ProfileTagVocabularyPageCursor>,
    skip: usize,
}

struct PendingProfilePage {
    target: ProfilePageTarget,
    request: VocabularyRequest<ProfileTagVocabularyPage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CountsTarget {
    generation: u64,
    page_epoch: u64,
    context: tags::TagSubmissionContext,
    profile_revision: Option<u64>,
    tag_ids: Vec<i64>,
    selected_count: usize,
}

struct PendingCountsPage {
    request_id: u64,
    target: CountsTarget,
}

struct PendingProfileLookup {
    generation: u64,
    page_epoch: u64,
    context: tags::TagSubmissionContext,
    profile_revision: Option<u64>,
    request: VocabularyRequest<ProfileTagVocabularyLookup>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProfileLookupTarget {
    generation: u64,
    page_epoch: u64,
    context: tags::TagSubmissionContext,
    profile_revision: Option<u64>,
    labels: Vec<String>,
}

struct PendingCategoryWrite {
    generation: u64,
    page_epoch: u64,
    context: tags::TagSubmissionContext,
    profile_revision: u64,
    label: String,
    category: ProfileTagCategory,
    request: VocabularyRequest<ProfileTagVocabularySetReceipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileLookupState {
    NotRequested,
    Pending,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileDefinitionStatus {
    Pending,
    Failed,
    KnownAbsent,
    Known(ProfileTagCategory),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DefinitionRemovalTarget {
    label: String,
    category: ProfileTagCategory,
}

/// Lightweight row payload retained by GPUI while a category drag is active.
#[derive(Clone)]
pub(super) struct DragCandidate {
    browser: Entity<Browser>,
    label: String,
    token: CandidateToken,
    preview: bool,
    generation: u64,
    page_epoch: u64,
    index: usize,
    profile_revision: Option<u64>,
}

#[derive(Clone)]
struct DragTarget {
    label: String,
    token: CandidateToken,
    preview: bool,
    generation: u64,
    page_epoch: u64,
    index: usize,
    context: tags::TagSubmissionContext,
    profile_revision: Option<u64>,
}

struct TagLibraryDragGhost;

impl Render for TagLibraryDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CandidateAction {
    AddSourceId(i64),
    RemoveSourceId(i64),
    AddLabel(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateActionError {
    InvalidSourceId,
    AssignmentStateUnavailable,
    AssignmentCountOutOfRange,
}

/// Whether the panel currently replaces the normal sidebar contents.
pub(super) fn is_open(browser: &Browser) -> bool {
    browser.tag_library.open
}

/// Toggle the panel and start its first bounded read when it opens.
pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_library.open = !browser.tag_library.open;
    if browser.tag_library.open {
        begin_refresh(browser);
        browser.start_source_registry_poll(cx);
    } else {
        browser.tag_library.clear_visible_work();
    }
    cx.notify();
}

/// Mark panel reads stale after selection, projection, or curation changes.
pub(super) fn invalidate(browser: &mut Browser) {
    if browser.tag_library.open {
        browser.tag_library.mark_dirty();
    }
}

/// Invalidate the panel after a durable curation receipt without closing it.
pub(super) fn invalidate_after_curation(browser: &mut Browser) {
    invalidate(browser);
}

/// Cancel panel reads on a source lifecycle replacement while retaining the chosen open state.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.tag_library.generation = browser.tag_library.generation.wrapping_add(1);
    browser.tag_library.target = None;
    browser.tag_library.source_pending = None;
    browser.tag_library.source_target = None;
    browser.tag_library.profile_pending = None;
    browser.tag_library.profile_target = None;
    browser.tag_library.counts_pending = None;
    browser.tag_library.counts_target = None;
    browser.tag_library.profile_lookup_pending = None;
    browser.tag_library.profile_lookup_target = None;
    browser.tag_library.profile_lookup_state = ProfileLookupState::NotRequested;
    browser.tag_library.source_ready = false;
    browser.tag_library.profile_ready = false;
    browser.tag_library.page_materialized = false;
    browser.tag_library.counts_requested = false;
    browser.tag_library.categories_requested = false;
    browser.tag_library.pager.clear_pages();
    browser.tag_library.assignment_counts.clear();
    browser.tag_library.counts_target = None;
    browser.tag_library.profile_categories.clear();
    browser.tag_library.profile_revision = None;
    browser.tag_library.metadata_retry_ticks = 0;
    browser.tag_library.profile_lookup_target = None;
    browser.tag_library.dragged = None;
    browser.tag_library.message = None;
    browser.tag_library.dirty = browser.tag_library.open;
}

/// Return whether panel work needs the shared source registry poll lane.
pub(super) fn has_pending(browser: &Browser) -> bool {
    let state = &browser.tag_library;
    browser.tag_dictionary.has_pending()
        || state.category_pending.is_some()
        || (state.open
            && (state.source_pending.is_some()
                || state.source_target.is_some()
                || state.profile_pending.is_some()
                || state.profile_target.is_some()
                || state.counts_pending.is_some()
                || state.counts_target.is_some()
                || state.profile_lookup_pending.is_some()
                || state.profile_lookup_target.is_some()
                || (state.dirty && browser.active_source.is_some())))
}

/// Poll one bounded panel lifecycle without creating a second timer.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let mut changed = false;
    changed |= poll_profile_page(browser);
    changed |= poll_profile_lookup(browser);
    changed |= poll_category_write(browser, cx);
    changed |= super::tag_dictionary::poll(browser, cx);
    if !browser.tag_library.open {
        return changed;
    }
    if browser.tag_library.dirty
        && browser.tag_library.target.is_none()
        && browser.tag_library.source_pending.is_none()
        && browser.tag_library.profile_pending.is_none()
        && browser.tag_library.counts_pending.is_none()
        && browser.tag_library.profile_lookup_pending.is_none()
        && browser.tag_library.counts_target.is_none()
        && browser.tag_library.profile_lookup_target.is_none()
    {
        changed |= capture_target(browser);
    }
    if browser.tag_library.source_pending.is_none()
        && let Some(target) = browser.tag_library.source_target.clone()
    {
        changed |= submit_source_page(browser, target, cx);
    }
    if browser.tag_library.profile_pending.is_none()
        && let Some(target) = browser.tag_library.profile_target.clone()
    {
        changed |= submit_profile_page(browser, target, cx);
    }
    changed |= poll_counts(browser);
    changed |= merge_if_ready(browser, cx);
    changed
}

fn begin_refresh(browser: &mut Browser) {
    browser.tag_library.mark_dirty();
}

fn capture_target(browser: &mut Browser) -> bool {
    let paths = match browser.capture_tag_target_paths(1) {
        Ok(paths) => paths,
        Err(error) => {
            browser.tag_library.dirty = false;
            browser.tag_library.message = Some(error);
            return true;
        }
    };
    if paths.is_empty() {
        browser.tag_library.dirty = false;
        browser.tag_library.message = Some(String::from("Select a sample to edit tags"));
        return true;
    }
    let Some(context) = tags::capture_tag_context(browser, paths) else {
        browser.tag_library.dirty = false;
        browser.tag_library.message = Some(String::from(
            "Tag library unavailable until the catalog is ready",
        ));
        return true;
    };
    let generation = browser.tag_library.generation;
    browser.tag_library.target = Some(PanelTarget {
        generation,
        context: context.clone(),
    });
    browser.tag_library.source_target = Some(SourcePageTarget {
        generation,
        context: context.clone(),
        after_cursor: None,
        skip: 0,
    });
    browser.tag_library.profile_target = Some(ProfilePageTarget {
        generation,
        context,
        expected_revision: None,
        after_cursor: None,
        skip: 0,
    });
    browser.tag_library.dirty = false;
    browser.tag_library.message = None;
    browser.tag_library.source_ready = false;
    browser.tag_library.profile_ready = false;
    browser.tag_library.page_materialized = false;
    browser.tag_library.pager.clear_pages();
    browser.tag_library.pager.current_checkpoint = Some(StreamCheckpoint::initial());
    browser.tag_library.assignment_counts.clear();
    browser.tag_library.profile_categories.clear();
    browser.tag_library.profile_lookup_state = ProfileLookupState::NotRequested;
    browser.tag_library.profile_revision = None;
    browser.tag_library.dragged = None;
    true
}

impl TagLibraryState {
    fn mark_dirty(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.target = None;
        self.source_target = None;
        self.profile_target = None;
        self.counts_target = None;
        self.source_ready = false;
        self.profile_ready = false;
        self.page_materialized = false;
        self.counts_requested = false;
        self.categories_requested = false;
        self.pager.clear_pages();
        self.assignment_counts.clear();
        self.profile_categories.clear();
        self.profile_lookup_target = None;
        self.profile_lookup_state = ProfileLookupState::NotRequested;
        self.profile_revision = None;
        self.metadata_retry_ticks = 0;
        self.dragged = None;
        self.message = None;
        self.dirty = true;
    }

    fn clear_visible_work(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.target = None;
        self.source_pending = None;
        self.source_target = None;
        self.profile_pending = None;
        self.profile_target = None;
        self.counts_pending = None;
        self.counts_target = None;
        self.profile_lookup_pending = None;
        self.profile_lookup_target = None;
        // Profile-owned category and dictionary requests outlive visible panel state.
        self.source_ready = false;
        self.profile_ready = false;
        self.page_materialized = false;
        self.counts_requested = false;
        self.categories_requested = false;
        self.dirty = false;
        self.pager.clear_pages();
        self.assignment_counts.clear();
        self.profile_categories.clear();
        self.profile_lookup_state = ProfileLookupState::NotRequested;
        self.profile_revision = None;
        self.metadata_retry_ticks = 0;
        self.dragged = None;
        self.message = None;
    }

    fn current_target(&self) -> Option<&PanelTarget> {
        self.target
            .as_ref()
            .filter(|target| target.generation == self.generation)
    }

    fn context_is_current(&self, browser: &Browser, context: &tags::TagSubmissionContext) -> bool {
        self.current_target()
            .is_some_and(|target| context_identity_matches(&target.context, context))
            && tags::tag_context_is_current(browser, context)
    }
}

fn context_identity_matches(
    left: &tags::TagSubmissionContext,
    right: &tags::TagSubmissionContext,
) -> bool {
    left.source_id == right.source_id
        && left.lifecycle_generation == right.lifecycle_generation
        && left.root == right.root
        && left.root_identity == right.root_identity
        && left.directory_generation == right.directory_generation
        && left.revision == right.revision
}

fn metadata_target_matches(
    generation: u64,
    page_epoch: u64,
    profile_revision: Option<u64>,
    context: &tags::TagSubmissionContext,
    current_generation: u64,
    current_page_epoch: u64,
    current_profile_revision: Option<u64>,
    current_context: &tags::TagSubmissionContext,
) -> bool {
    generation == current_generation
        && page_epoch == current_page_epoch
        && profile_revision == current_profile_revision
        && context_identity_matches(context, current_context)
}

fn drag_target_is_current(
    target: &DragTarget,
    generation: u64,
    page_epoch: u64,
    candidate: Option<CandidateToken>,
    profile_revision: Option<u64>,
    context: Option<&tags::TagSubmissionContext>,
) -> bool {
    drag_metadata_is_current(
        target.generation,
        target.page_epoch,
        &target.token,
        target.profile_revision,
        generation,
        page_epoch,
        candidate,
        profile_revision,
    ) && context.is_some_and(|context| context_identity_matches(context, &target.context))
}

fn drag_metadata_is_current(
    target_generation: u64,
    target_page_epoch: u64,
    target_token: &CandidateToken,
    target_profile_revision: Option<u64>,
    generation: u64,
    page_epoch: u64,
    candidate: Option<CandidateToken>,
    profile_revision: Option<u64>,
) -> bool {
    target_generation == generation
        && target_page_epoch == page_epoch
        && candidate.as_ref() == Some(target_token)
        && target_profile_revision == profile_revision
}

fn drag_payload_matches_target(
    candidate: &DragCandidate,
    target: &DragTarget,
    browser_entity_id: gpui::EntityId,
) -> bool {
    candidate.browser.entity_id() == browser_entity_id
        && candidate.label == target.label
        && candidate.token == target.token
        && candidate.preview == target.preview
        && candidate.generation == target.generation
        && candidate.page_epoch == target.page_epoch
        && candidate.index == target.index
        && candidate.profile_revision == target.profile_revision
}

fn drag_target_paths_match(target: &DragTarget, current_paths: &[std::path::PathBuf]) -> bool {
    target.context.paths == current_paths
}

fn candidate_action(
    candidate: &MergedCandidate,
    assignment_counts: &BTreeMap<i64, u64>,
    selected_count: usize,
) -> Result<CandidateAction, CandidateActionError> {
    match candidate {
        MergedCandidate::Source { tag_id, .. } => {
            if *tag_id <= 0 {
                return Err(CandidateActionError::InvalidSourceId);
            }
            let Some(count) = assignment_counts.get(tag_id).copied() else {
                return Err(CandidateActionError::AssignmentStateUnavailable);
            };
            if count > selected_count as u64 {
                return Err(CandidateActionError::AssignmentCountOutOfRange);
            }
            if count == selected_count as u64 {
                Ok(CandidateAction::RemoveSourceId(*tag_id))
            } else {
                Ok(CandidateAction::AddSourceId(*tag_id))
            }
        }
        MergedCandidate::Profile { label } => Ok(CandidateAction::AddLabel(label.clone())),
        MergedCandidate::Default(label) => Ok(CandidateAction::AddLabel((*label).to_owned())),
    }
}

fn submit_source_page(
    browser: &mut Browser,
    target: SourcePageTarget,
    cx: &mut Context<Browser>,
) -> bool {
    if !browser.tag_library.open
        || target.generation != browser.tag_library.generation
        || browser.tag_library.target.as_ref().is_none_or(|current| {
            current.generation != target.generation || current.context != target.context
        })
    {
        return false;
    }
    let request_id = browser.next_tag_request_id();
    let command = CatalogCommand::RequestTagPage {
        source_id: target.context.source_id.clone(),
        lifecycle_generation: target.context.lifecycle_generation,
        expected_revision: target.context.revision,
        request_id,
        query: CatalogTagPageQuery::VocabularyPrefix {
            prefix: String::new(),
        },
        after_cursor: target.after_cursor,
        page_limit: PANEL_PAGE_SIZE,
    };
    match catalog::submit_catalog_command(command, cx) {
        Ok(()) => {
            browser.tag_library.source_target = None;
            browser.tag_library.source_pending = Some(PendingSourcePage { request_id, target });
            true
        }
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
            browser.tag_library.message = Some(String::from("Tags waiting for catalog…"));
            true
        }
        Err(error) => {
            browser.tag_library.source_target = None;
            browser.tag_library.pager.source_page = None;
            browser.tag_library.source_ready = true;
            browser.tag_library.message = Some(format!("Could not load source tags: {error}"));
            true
        }
    }
}

fn submit_profile_page(
    browser: &mut Browser,
    target: ProfilePageTarget,
    cx: &mut Context<Browser>,
) -> bool {
    if !browser.tag_library.open
        || target.generation != browser.tag_library.generation
        || browser.tag_library.target.as_ref().is_none_or(|current| {
            current.generation != target.generation || current.context != target.context
        })
    {
        return false;
    }
    let Some(global) = cx
        .has_global::<SourceRegistryGlobal>()
        .then(|| cx.global::<SourceRegistryGlobal>())
    else {
        browser.tag_library.profile_target = None;
        browser.tag_library.pager.profile_page = None;
        browser.tag_library.profile_ready = true;
        browser.tag_library.message = Some(String::from("Profile tags unavailable"));
        return true;
    };
    match global.profile_tag_vocabulary_page(
        target.expected_revision,
        String::new(),
        target.after_cursor.clone(),
        PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE,
    ) {
        Ok(request) => {
            browser.tag_library.profile_target = None;
            browser.tag_library.profile_pending = Some(PendingProfilePage { target, request });
            true
        }
        Err(VocabularySubmitError::Full | VocabularySubmitError::Initializing) => {
            browser.tag_library.message = Some(String::from("Profile tags waiting…"));
            true
        }
        Err(error) => {
            browser.tag_library.profile_target = None;
            browser.tag_library.pager.profile_page = None;
            browser.tag_library.profile_ready = true;
            browser.tag_library.message = Some(format!("Could not load profile tags: {error}"));
            true
        }
    }
}

fn poll_profile_page(browser: &mut Browser) -> bool {
    let Some(mut pending) = browser.tag_library.profile_pending.take() else {
        return false;
    };
    let result = match pending.request.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            browser.tag_library.profile_pending = Some(pending);
            return false;
        }
        Err(TryRecvError::Disconnected) => {
            if pending.target.generation == browser.tag_library.generation {
                browser.tag_library.profile_ready = true;
                browser.tag_library.message = Some(String::from("Profile tags disconnected"));
            }
            return true;
        }
    };
    if pending.target.generation != browser.tag_library.generation {
        browser.tag_library.mark_dirty();
        return true;
    }
    if !browser
        .tag_library
        .context_is_current(browser, &pending.target.context)
    {
        browser.tag_library.mark_dirty();
        return true;
    }
    match result {
        Ok(page) if page.entries.len() <= PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE => {
            if restart_for_profile_revision_change(
                &mut browser.tag_library,
                pending.target.expected_revision,
                page.revision,
            ) {
                return true;
            }
            browser.tag_library.profile_revision = Some(page.revision);
            for entry in &page.entries {
                browser
                    .tag_library
                    .profile_categories
                    .insert(entry.label.clone(), entry.category);
            }
            browser.tag_library.pager.profile_page = Some(ProfilePageWindow {
                revision: page.revision,
                entries: page.entries,
                next_cursor: page.next_cursor,
                page_start: pending.target.after_cursor,
                offset: pending.target.skip,
            });
            browser.tag_library.profile_ready = true;
            browser.tag_library.message = None;
            true
        }
        Ok(_) => {
            browser.tag_library.pager.profile_page = None;
            browser.tag_library.profile_ready = true;
            browser.tag_library.message = Some(String::from("Profile tag page exceeded its bound"));
            true
        }
        Err(error) => {
            browser.tag_library.pager.profile_page = None;
            browser.tag_library.profile_ready = true;
            browser.tag_library.message = Some(format!("Could not read profile tags: {error}"));
            true
        }
    }
}

fn restart_for_profile_revision_change(
    state: &mut TagLibraryState,
    expected_revision: Option<u64>,
    actual_revision: u64,
) -> bool {
    if expected_revision.is_none_or(|revision| revision == actual_revision) {
        return false;
    }
    state.mark_dirty();
    state.message = Some(String::from(
        "Profile tag vocabulary changed; refreshing the tag library",
    ));
    true
}

fn merge_if_ready(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser.tag_library.page_materialized
        && browser.tag_library.source_pending.is_none()
        && browser.tag_library.profile_pending.is_none()
        && browser.tag_library.counts_pending.is_none()
        && browser.tag_library.profile_lookup_pending.is_none()
    {
        return start_page_metadata_reads(browser, cx);
    }
    if !browser.tag_library.open
        || !browser.tag_library.source_ready
        || !browser.tag_library.profile_ready
        || browser.tag_library.source_pending.is_some()
        || browser.tag_library.profile_pending.is_some()
        || browser.tag_library.source_target.is_some()
        || browser.tag_library.profile_target.is_some()
    {
        return false;
    }
    let merge = merge_ready_page(&mut browser.tag_library.pager, "", PANEL_PAGE_SIZE);
    let mut changed = false;
    if let Some(after_cursor) = merge.source_after
        && browser.tag_library.pager.merged_page.len() < PANEL_PAGE_SIZE
        && let Some(target) = browser.tag_library.current_target()
    {
        browser.tag_library.source_target = Some(SourcePageTarget {
            generation: target.generation,
            context: target.context.clone(),
            after_cursor: Some(after_cursor),
            skip: 0,
        });
        browser.tag_library.source_ready = false;
        changed = true;
    }
    if let Some(after_cursor) = merge.profile_after
        && browser.tag_library.pager.merged_page.len() < PANEL_PAGE_SIZE
        && let Some(target) = browser.tag_library.current_target()
    {
        browser.tag_library.profile_target = Some(ProfilePageTarget {
            generation: target.generation,
            context: target.context.clone(),
            expected_revision: browser.tag_library.profile_revision,
            after_cursor: Some(after_cursor),
            skip: 0,
        });
        browser.tag_library.profile_ready = false;
        changed = true;
    }
    if browser.tag_library.source_target.is_none()
        && browser.tag_library.profile_target.is_none()
        && !browser.tag_library.page_materialized
    {
        browser.tag_library.page_materialized = true;
        changed = true;
        changed |= start_page_metadata_reads(browser, cx);
    }
    changed
}

fn start_page_metadata_reads(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(target) = browser.tag_library.current_target().cloned() else {
        return false;
    };
    if browser.tag_library.metadata_retry_ticks > 0 {
        browser.tag_library.metadata_retry_ticks -= 1;
        return false;
    }
    let mut tag_ids = Vec::new();
    let mut labels = Vec::new();
    for candidate in &browser.tag_library.pager.merged_page {
        match candidate {
            MergedCandidate::Source {
                tag_id,
                exact_label,
                ..
            } if *tag_id > 0 => {
                if !tag_ids.contains(tag_id) {
                    tag_ids.push(*tag_id);
                }
                if let Some(label) = exact_label.as_deref()
                    && is_profile_label(label)
                    && !labels.iter().any(|known: &String| known == label)
                {
                    labels.push(label.to_owned());
                }
            }
            MergedCandidate::Profile { label } if is_profile_label(label) => {
                if !labels.iter().any(|known| known == label) {
                    labels.push(label.clone());
                }
            }
            MergedCandidate::Profile { .. } => {}
            MergedCandidate::Default(label) => {
                if !labels.iter().any(|known| known == label) {
                    labels.push((*label).to_owned());
                }
            }
            MergedCandidate::Source { .. } => {}
        }
        if tag_ids.len() >= PANEL_PAGE_SIZE || labels.len() >= MAX_CATEGORY_LOOKUP_LABELS {
            break;
        }
    }
    let page_epoch = browser.tag_library.pager.page_epoch;
    let profile_revision = browser.tag_library.profile_revision;
    if browser
        .tag_library
        .counts_target
        .as_ref()
        .is_some_and(|retry| {
            !metadata_target_matches(
                retry.generation,
                retry.page_epoch,
                retry.profile_revision,
                &retry.context,
                target.generation,
                page_epoch,
                profile_revision,
                &target.context,
            )
        })
    {
        browser.tag_library.counts_target = None;
        browser.tag_library.counts_requested = false;
    }
    if browser
        .tag_library
        .profile_lookup_target
        .as_ref()
        .is_some_and(|retry| {
            !metadata_target_matches(
                retry.generation,
                retry.page_epoch,
                retry.profile_revision,
                &retry.context,
                target.generation,
                page_epoch,
                profile_revision,
                &target.context,
            )
        })
    {
        browser.tag_library.profile_lookup_target = None;
        browser.tag_library.categories_requested = false;
    }
    let mut changed = false;
    if tag_ids.is_empty() {
        browser.tag_library.counts_requested = true;
        browser.tag_library.counts_target = None;
    } else if !browser.tag_library.counts_requested && browser.tag_library.counts_pending.is_none()
    {
        let count_target = CountsTarget {
            generation: target.generation,
            page_epoch,
            context: target.context.clone(),
            profile_revision,
            tag_ids: tag_ids.clone(),
            selected_count: target.context.paths.len(),
        };
        let request_id = browser.next_tag_request_id();
        let command = CatalogCommand::RequestTagPage {
            source_id: target.context.source_id.clone(),
            lifecycle_generation: target.context.lifecycle_generation,
            expected_revision: target.context.revision,
            request_id,
            query: CatalogTagPageQuery::SelectedTagIds {
                relative_paths: target.context.paths.clone(),
                tag_ids: tag_ids.clone(),
            },
            after_cursor: None,
            page_limit: PANEL_PAGE_SIZE,
        };
        match catalog::submit_catalog_command(command, cx) {
            Ok(()) => {
                browser.tag_library.counts_requested = true;
                browser.tag_library.counts_pending = Some(PendingCountsPage {
                    request_id,
                    target: CountsTarget {
                        generation: count_target.generation,
                        page_epoch: count_target.page_epoch,
                        context: count_target.context.clone(),
                        profile_revision: count_target.profile_revision,
                        tag_ids: count_target.tag_ids.clone(),
                        selected_count: count_target.selected_count,
                    },
                });
                browser.tag_library.counts_target = None;
                changed = true;
            }
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
                browser.tag_library.counts_target = Some(count_target);
                browser.tag_library.metadata_retry_ticks = 5;
                browser.tag_library.message = Some(String::from("Tag counts waiting…"));
                changed = true;
            }
            Err(error) => {
                browser.tag_library.counts_target = None;
                browser.tag_library.counts_requested = true;
                browser.tag_library.message = Some(format!("Could not load tag counts: {error}"));
                changed = true;
            }
        }
    }
    if labels.is_empty() {
        browser.tag_library.categories_requested = true;
        browser.tag_library.profile_lookup_target = None;
        browser.tag_library.profile_lookup_state = ProfileLookupState::Ready;
    } else if !browser.tag_library.categories_requested
        && browser.tag_library.profile_lookup_pending.is_none()
    {
        browser.tag_library.profile_lookup_state = ProfileLookupState::Pending;
        let lookup_target = ProfileLookupTarget {
            generation: target.generation,
            page_epoch,
            context: target.context.clone(),
            profile_revision,
            labels,
        };
        let Some(global) = cx
            .has_global::<SourceRegistryGlobal>()
            .then(|| cx.global::<SourceRegistryGlobal>())
        else {
            browser.tag_library.categories_requested = true;
            browser.tag_library.profile_lookup_target = None;
            browser.tag_library.profile_lookup_state = ProfileLookupState::Failed;
            return changed;
        };
        match global.lookup_profile_tag_categories(lookup_target.labels.clone()) {
            Ok(request) => {
                browser.tag_library.categories_requested = true;
                browser.tag_library.profile_lookup_pending = Some(PendingProfileLookup {
                    generation: target.generation,
                    page_epoch,
                    context: target.context,
                    profile_revision,
                    request,
                });
                browser.tag_library.profile_lookup_target = None;
                changed = true;
            }
            Err(error @ (VocabularySubmitError::Full | VocabularySubmitError::Initializing)) => {
                browser.tag_library.profile_lookup_target = Some(lookup_target);
                browser.tag_library.profile_lookup_state = ProfileLookupState::Pending;
                browser.tag_library.metadata_retry_ticks = 5;
                browser.tag_library.message =
                    Some(format!("Could not load tag categories: {error}"));
                changed = true;
            }
            Err(error) => {
                browser.tag_library.profile_lookup_target = None;
                browser.tag_library.categories_requested = true;
                browser.tag_library.profile_lookup_state = ProfileLookupState::Failed;
                browser.tag_library.message =
                    Some(format!("Could not load tag categories: {error}"));
                changed = true;
            }
        }
    }
    changed
}

fn poll_counts(browser: &mut Browser) -> bool {
    let Some(pending) = browser.tag_library.counts_pending.take() else {
        return false;
    };
    if pending.target.generation != browser.tag_library.generation {
        browser.tag_library.mark_dirty();
        return true;
    }
    if pending.target.page_epoch != browser.tag_library.pager.page_epoch {
        browser.tag_library.counts_requested = false;
        browser.tag_library.counts_pending = Some(pending);
        return false;
    }
    if pending.target.profile_revision != browser.tag_library.profile_revision {
        browser.tag_library.mark_dirty();
        return true;
    }
    if !browser
        .tag_library
        .context_is_current(browser, &pending.target.context)
    {
        browser.tag_library.mark_dirty();
        return true;
    }
    browser.tag_library.counts_pending = Some(pending);
    false
}

fn poll_profile_lookup(browser: &mut Browser) -> bool {
    let Some(mut pending) = browser.tag_library.profile_lookup_pending.take() else {
        return false;
    };
    let result = match pending.request.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            browser.tag_library.profile_lookup_pending = Some(pending);
            return false;
        }
        Err(TryRecvError::Disconnected) => {
            if pending.generation == browser.tag_library.generation
                && pending.page_epoch == browser.tag_library.pager.page_epoch
                && pending.profile_revision == browser.tag_library.profile_revision
                && browser
                    .tag_library
                    .context_is_current(browser, &pending.context)
            {
                browser.tag_library.categories_requested = true;
                browser.tag_library.profile_lookup_state = ProfileLookupState::Failed;
                browser.tag_library.message = Some(String::from("Tag categories disconnected"));
            }
            return true;
        }
    };
    if pending.generation != browser.tag_library.generation {
        browser.tag_library.mark_dirty();
        return true;
    }
    if pending.page_epoch != browser.tag_library.pager.page_epoch {
        browser.tag_library.categories_requested = false;
        browser.tag_library.profile_lookup_target = None;
        browser.tag_library.profile_lookup_state = ProfileLookupState::NotRequested;
        return true;
    }
    if pending.profile_revision != browser.tag_library.profile_revision {
        browser.tag_library.mark_dirty();
        return true;
    }
    if !browser
        .tag_library
        .context_is_current(browser, &pending.context)
    {
        browser.tag_library.mark_dirty();
        return true;
    }
    match result {
        Ok(lookup) => {
            if pending
                .profile_revision
                .is_some_and(|revision| revision != lookup.revision)
            {
                browser.tag_library.mark_dirty();
                return true;
            }
            browser.tag_library.profile_revision = Some(lookup.revision);
            browser.tag_library.profile_lookup_target = None;
            browser.tag_library.profile_lookup_state = ProfileLookupState::Ready;
            for entry in lookup.entries {
                browser
                    .tag_library
                    .profile_categories
                    .insert(entry.label, entry.category);
            }
            true
        }
        Err(error) => {
            browser.tag_library.categories_requested = true;
            browser.tag_library.profile_lookup_target = None;
            browser.tag_library.profile_lookup_state = ProfileLookupState::Failed;
            browser.tag_library.message = Some(format!("Could not load tag categories: {error}"));
            true
        }
    }
}

fn poll_category_write(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(mut pending) = browser.tag_library.category_pending.take() else {
        return false;
    };
    let result = match pending.request.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            browser.tag_library.category_pending = Some(pending);
            return false;
        }
        Err(TryRecvError::Disconnected) => {
            let current = pending.generation == browser.tag_library.generation
                && pending.page_epoch == browser.tag_library.pager.page_epoch
                && browser.tag_library.profile_revision == Some(pending.profile_revision)
                && browser
                    .tag_library
                    .context_is_current(browser, &pending.context);
            if current {
                browser.tag_library.message = Some(String::from("Tag category write disconnected"));
            }
            return true;
        }
    };
    let current = pending.generation == browser.tag_library.generation
        && pending.page_epoch == browser.tag_library.pager.page_epoch
        && browser.tag_library.profile_revision == Some(pending.profile_revision)
        && browser
            .tag_library
            .context_is_current(browser, &pending.context);
    match result {
        Ok(receipt) => {
            if !current {
                if browser.tag_library.open {
                    browser.tag_library.mark_dirty();
                    browser.start_source_registry_poll(cx);
                }
                return true;
            }
            browser.tag_library.profile_revision = Some(receipt.revision);
            browser
                .tag_library
                .profile_categories
                .insert(pending.label, pending.category);
            browser.tag_library.message = Some(String::from("Tag category saved"));
            if pending.generation == browser.tag_library.generation
                && browser
                    .tag_library
                    .context_is_current(browser, &pending.context)
            {
                begin_refresh(browser);
                browser.start_source_registry_poll(cx);
            }
            true
        }
        Err(error) => {
            if current {
                browser.tag_library.message = Some(format!("Could not save tag category: {error}"));
            }
            true
        }
    }
}

/// Invalidate visible pages after a profile dictionary mutation without touching its receipt.
pub(super) fn dictionary_mutation_finished(
    browser: &mut Browser,
    message: &str,
    cx: &mut Context<Browser>,
) {
    if browser.tag_library.open {
        browser.tag_library.mark_dirty();
        browser.start_source_registry_poll(cx);
    }
    super::tag_completion::invalidate_profile_vocabulary(browser, cx);
    browser.tag_library.message = Some(message.to_owned());
}

/// Invalidate visible pages after a profile dictionary mutation failure.
pub(super) fn dictionary_mutation_failed(
    browser: &mut Browser,
    message: String,
    cx: &mut Context<Browser>,
) {
    if browser.tag_library.open {
        browser.tag_library.mark_dirty();
        browser.start_source_registry_poll(cx);
    }
    super::tag_completion::invalidate_profile_vocabulary(browser, cx);
    browser.tag_library.message = Some(message);
}

/// Route a source tag page or count page to the matching panel request.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    let (source_id, lifecycle_generation, request_id) = match event {
        CatalogEvent::TagPageReady {
            source_id,
            lifecycle_generation,
            request_id,
            ..
        }
        | CatalogEvent::TagPageFailed {
            source_id,
            lifecycle_generation,
            request_id,
            ..
        } => (source_id, *lifecycle_generation, *request_id),
        _ => return false,
    };
    if browser
        .tag_library
        .counts_pending
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
    {
        return apply_counts_event(browser, event, source_id, lifecycle_generation);
    }
    let Some(pending) = browser.tag_library.source_pending.as_ref() else {
        return false;
    };
    if pending.request_id != request_id
        || pending.target.context.source_id != *source_id
        || pending.target.context.lifecycle_generation != lifecycle_generation
    {
        return false;
    }
    let pending = browser
        .tag_library
        .source_pending
        .take()
        .expect("tag-library source request was checked");
    if pending.target.generation != browser.tag_library.generation
        || !browser
            .tag_library
            .context_is_current(browser, &pending.target.context)
    {
        browser.tag_library.mark_dirty();
        return true;
    }
    match event {
        CatalogEvent::TagPageReady {
            root_identity,
            revision,
            page,
            ..
        } if tags::source_page_reply_is_coherent(
            &pending.target.context,
            source_id,
            lifecycle_generation,
            root_identity,
            *revision,
            page.revision,
        ) && page.entries.len() <= PANEL_PAGE_SIZE =>
        {
            browser.tag_library.pager.source_page = Some(super::tag_pager::SourcePageWindow {
                revision: page.revision,
                entries: page.entries.clone(),
                next_cursor: page.next_cursor,
                page_start: pending.target.after_cursor,
                offset: pending.target.skip,
            });
            browser.tag_library.source_ready = true;
            browser.tag_library.message = None;
        }
        CatalogEvent::TagPageFailed {
            expected_revision,
            observed_revision,
            error,
            ..
        } if *expected_revision == pending.target.context.revision => {
            if observed_revision.is_some_and(|revision| revision != *expected_revision) {
                browser.tag_library.source_ready = true;
                browser.tag_library.pager.source_page = None;
                browser.tag_library.message = Some(String::from(
                    "Source tag vocabulary changed; refresh the selection to retry",
                ));
            } else {
                browser.tag_library.source_ready = true;
                browser.tag_library.pager.source_page = None;
                browser.tag_library.message = Some(format!("Could not load source tags: {error}"));
            }
        }
        _ => {
            browser.tag_library.source_ready = true;
            browser.tag_library.pager.source_page = None;
            browser.tag_library.message = Some(String::from(
                "Source tag page was stale; refresh the selection to retry",
            ));
        }
    }
    let _ = merge_if_ready(browser, cx);
    true
}

fn apply_counts_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    source_id: &yield_library::sample_sources::SourceId,
    lifecycle_generation: u64,
) -> bool {
    let Some(pending) = browser.tag_library.counts_pending.take() else {
        return false;
    };
    if pending.target.context.source_id != *source_id
        || pending.target.context.lifecycle_generation != lifecycle_generation
    {
        browser.tag_library.counts_pending = Some(pending);
        return false;
    }
    if pending.target.generation != browser.tag_library.generation {
        browser.tag_library.dirty = true;
        return true;
    }
    if pending.target.page_epoch != browser.tag_library.pager.page_epoch {
        browser.tag_library.counts_requested = false;
        return true;
    }
    if !browser
        .tag_library
        .context_is_current(browser, &pending.target.context)
    {
        browser.tag_library.dirty = true;
        return true;
    }
    if pending.target.profile_revision != browser.tag_library.profile_revision {
        browser.tag_library.dirty = true;
        return true;
    }
    match event {
        CatalogEvent::TagPageReady {
            root_identity,
            revision,
            page,
            ..
        } if tags::source_page_reply_is_coherent(
            &pending.target.context,
            source_id,
            lifecycle_generation,
            root_identity,
            *revision,
            page.revision,
        ) && page.revision == pending.target.context.revision
            && page.entries.len() <= PANEL_PAGE_SIZE
            && valid_count_entries(
                &page.entries,
                &pending.target.tag_ids,
                pending.target.selected_count,
            ) =>
        {
            browser.tag_library.assignment_counts.clear();
            for entry in &page.entries {
                if let Some(count) = entry.assignment_count {
                    browser
                        .tag_library
                        .assignment_counts
                        .insert(entry.id, count);
                }
            }
            browser.tag_library.message = None;
        }
        CatalogEvent::TagPageFailed {
            expected_revision,
            observed_revision,
            error,
            ..
        } if *expected_revision == pending.target.context.revision => {
            if observed_revision.is_some_and(|revision| revision != *expected_revision) {
                browser.tag_library.message = Some(String::from(
                    "Tag assignment counts were stale; refresh the selection to retry",
                ));
            } else {
                browser.tag_library.message = Some(format!("Could not load tag counts: {error}"));
            }
        }
        _ => {
            browser.tag_library.message = Some(String::from(
                "Tag counts were stale; refresh the selection to retry",
            ));
        }
    }
    browser.tag_library.counts_requested = true;
    browser.tag_library.counts_target = None;
    true
}

fn valid_count_entries(
    entries: &[SourceTagPageEntry],
    requested: &[i64],
    selected_count: usize,
) -> bool {
    let requested: BTreeSet<i64> = requested.iter().copied().collect();
    let mut seen = BTreeSet::new();
    entries.iter().all(|entry| {
        entry.id > 0
            && entry
                .assignment_count
                .is_some_and(|count| count <= selected_count as u64)
            && requested.contains(&entry.id)
            && seen.insert(entry.id)
    })
}

/// Render the bounded Tag Editor panel.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let header = div()
        .flex()
        .flex_none()
        .w_full()
        .h(px(
            theme.spacing.row_height + theme.spacing.panel_padding * 2.0
        ))
        .items_center()
        .px(px(theme.spacing.panel_padding))
        .bg(color(theme.colors.surface_overlay))
        .border_b(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .child("TAG EDITOR")
        .child(div().flex_1())
        .child(close_button(theme.clone(), cx));
    let editor = super::tags::render(browser, theme.clone(), cx).into_any_element();
    let selected = super::tag_chips::render(browser, theme.clone(), cx).into_any_element();
    let body = render_categories(browser, theme.clone(), cx).into_any_element();
    let controls = render_page_controls(browser, theme.clone(), cx).into_any_element();
    let status = browser.tag_library.message.clone().map(|message| {
        div()
            .flex_none()
            .w_full()
            .h(px(theme.spacing.row_height))
            .px(px(theme.spacing.panel_padding))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.warning))
            .child(message)
    });
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(theme.spacing.sidebar_width))
        .h_full()
        .bg(color(theme.colors.surface_raised))
        .border_r(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .child(header)
        .child(super::tag_dictionary::render_undo(
            browser,
            theme.clone(),
            cx,
        ))
        .child(editor)
        .child(selected)
        .child(body)
        .child(controls)
        .children(status)
}

fn close_button(theme: Theme, cx: &mut Context<Browser>) -> impl gpui::IntoElement {
    div()
        .id("tag-library-close")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(theme.spacing.control_height))
        .h(px(theme.spacing.control_height))
        .cursor_pointer()
        .text_size(px(theme.typography.title_size))
        .text_color(color(theme.colors.text_muted))
        .hover(|style| style.bg(color(theme.colors.selection_hover_fill)))
        .active(|style| style.bg(color(theme.interaction.pressed_fill)))
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
            toggle(view, cx);
        }))
        .child("×")
}

fn render_categories(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let mut groups = Vec::with_capacity(CATEGORY_COUNT);
    for category in TAG_CATEGORIES {
        let index = category.order();
        let expanded = browser.tag_library.expanded[index];
        let header_hover = color(theme.colors.selection_hover_fill);
        let header = div()
            .id(("tag-library-category", index))
            .flex()
            .flex_none()
            .w_full()
            .h(px(theme.spacing.row_height))
            .items_center()
            .px(px(theme.spacing.panel_padding))
            .cursor_pointer()
            .text_size(px(theme.typography.small_size))
            .text_color(color(if category == TagCategory::PlaybackType {
                theme.colors.text_disabled
            } else {
                theme.colors.accent
            }))
            .hover(move |style| style.bg(header_hover))
            .on_drop(cx.listener(move |view, candidate: &DragCandidate, _, cx| {
                view.drop_tag_category(category, candidate, cx);
            }))
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.toggle_tag_library_category(index, cx);
            }))
            .child(if expanded { "▾" } else { "▸" })
            .child(
                div()
                    .flex_1()
                    .pl(px(theme.spacing.control_gap))
                    .child(category.label()),
            );
        let rows = if expanded {
            browser
                .tag_library
                .pager
                .merged_page
                .iter()
                .enumerate()
                .filter(|(_, candidate)| effective_category(browser, candidate) == category)
                .map(|(index, candidate)| {
                    render_candidate(browser, candidate, index, &theme, cx).into_any_element()
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        groups.push(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .w_full()
                .child(header)
                .children(rows),
        );
    }
    div()
        .id("tag-library-categories")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scroll()
        .track_scroll(&browser.tag_library.scroll_handle)
        .children(groups)
}

fn render_page_controls(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let has_previous = !browser.tag_library.pager.page_history.is_empty();
    let has_next = browser.tag_library.pager.page_has_next;
    if !has_previous && !has_next {
        return div().into_any_element();
    }
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let first = div()
        .id("tag-library-first")
        .flex_1()
        .h(px(theme.spacing.row_height))
        .items_center()
        .justify_center()
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
            view.request_tag_library_first(cx);
        }))
        .child("First");
    let previous = div()
        .id("tag-library-previous")
        .flex_1()
        .h(px(theme.spacing.row_height))
        .items_center()
        .justify_center()
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .cursor_pointer()
        .hover(move |style| style.bg(color(theme.colors.selection_hover_fill)))
        .active(move |style| style.bg(color(theme.interaction.pressed_fill)))
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
            view.request_tag_library_previous(cx);
        }))
        .child("Previous");
    let next = div()
        .id("tag-library-next")
        .flex_1()
        .h(px(theme.spacing.row_height))
        .items_center()
        .justify_center()
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .cursor_pointer()
        .hover(move |style| style.bg(color(theme.colors.selection_hover_fill)))
        .active(move |style| style.bg(color(theme.interaction.pressed_fill)))
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
            view.request_tag_library_next(cx);
        }))
        .child("Next");
    div()
        .flex()
        .flex_none()
        .w_full()
        .gap(px(theme.spacing.control_gap))
        .px(px(theme.spacing.panel_padding))
        .pt(px(theme.spacing.control_gap))
        .children([first, previous, next])
        .into_any_element()
}

fn render_candidate(
    browser: &Browser,
    candidate: &MergedCandidate,
    index: usize,
    theme: &Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let label = candidate.display_label();
    let token = candidate.token();
    let candidate_for_click = candidate.clone();
    let epoch = browser.tag_library.pager.page_epoch;
    let generation = browser.tag_library.generation;
    let assignment = assignment_label(browser, candidate);
    let movable = movable_candidate(candidate);
    let profile_revision = browser.tag_library.profile_revision;
    let removal_target = definition_removal_target(
        candidate,
        &browser.tag_library.profile_categories,
        browser.tag_library.profile_lookup_state,
    );
    let drag_label = category_label(candidate)
        .map(str::to_owned)
        .unwrap_or_else(|| candidate.display_label());
    let toggle_enabled = match candidate {
        MergedCandidate::Source { tag_id, .. } => {
            *tag_id > 0 && browser.tag_library.assignment_counts.contains_key(tag_id)
        }
        MergedCandidate::Profile { .. } | MergedCandidate::Default(_) => true,
    };
    let drag = browser
        .tag_library
        .current_target()
        .filter(|_| movable)
        .map(|_| DragCandidate {
            browser: cx.entity(),
            label: drag_label,
            token: token.clone(),
            preview: matches!(
                candidate,
                MergedCandidate::Source {
                    exact_label: None,
                    ..
                }
            ),
            generation,
            page_epoch: epoch,
            index,
            profile_revision: browser.tag_library.profile_revision,
        });
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let remove_pending =
        browser.tag_dictionary.has_pending() || browser.tag_library.category_pending.is_some();
    let remove_button = removal_target.map(|target| {
        let remove_enabled = !remove_pending;
        let remove_token = token.clone();
        let remove_target = target.clone();
        let remove_hover = color(theme.colors.selection_hover_fill);
        let remove_pressed = color(theme.interaction.pressed_fill);
        div()
            .id(("tag-library-remove-definition", index))
            .flex_none()
            .w(px(theme.spacing.control_height))
            .h(px(theme.spacing.control_height))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(theme.typography.small_size))
            .text_color(color(if remove_enabled {
                theme.colors.text_muted
            } else {
                theme.colors.text_disabled
            }))
            .when(remove_enabled, |element| {
                element
                    .cursor_pointer()
                    .hover(move |style| style.bg(remove_hover))
                    .active(move |style| style.bg(remove_pressed))
            })
            .when(browser.hover_help_visible(), |element| {
                element.tooltip(super::tag_dictionary::remove_definition_tooltip)
            })
            .on_click(cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                if remove_enabled {
                    view.remove_tag_library_definition(
                        index,
                        remove_token.clone(),
                        generation,
                        epoch,
                        profile_revision,
                        remove_target.clone(),
                        cx,
                    );
                }
            }))
            .child("×")
    });
    div()
        .id(("tag-library-row", index))
        .flex()
        .flex_none()
        .w_full()
        .h(px(theme.spacing.row_height))
        .items_center()
        .px(px(theme.spacing.panel_padding + theme.spacing.row_padding_x))
        .cursor(if toggle_enabled {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        })
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .when_some(drag, |this, drag| {
            this.on_drag(drag, |drag, _, _, cx| {
                let _ = drag.browser.update(cx, |view, cx| {
                    view.begin_tag_category_drag(drag.clone(), cx);
                });
                cx.new(|_| TagLibraryDragGhost)
            })
        })
        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
            view.toggle_tag_library_candidate(
                index,
                candidate_for_click.clone(),
                generation,
                epoch,
                cx,
            );
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(theme.typography.small_size))
                .text_color(color(if toggle_enabled {
                    theme.colors.text_primary
                } else {
                    theme.colors.text_disabled
                }))
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .px(px(theme.spacing.row_padding_x))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(assignment),
        )
        .children(remove_button)
}

fn assignment_label(browser: &Browser, candidate: &MergedCandidate) -> &'static str {
    let MergedCandidate::Source { tag_id, .. } = candidate else {
        return "—";
    };
    let Some(count) = browser.tag_library.assignment_counts.get(tag_id) else {
        return "…";
    };
    if *count
        == browser
            .tag_library
            .current_target()
            .map(|target| target.context.paths.len() as u64)
            .unwrap_or_default()
    {
        "All"
    } else if *count == 0 {
        "None"
    } else {
        "Mixed"
    }
}

fn effective_category(browser: &Browser, candidate: &MergedCandidate) -> TagCategory {
    let label = match candidate {
        MergedCandidate::Default(label) => *label,
        MergedCandidate::Profile { label } => label.as_str(),
        MergedCandidate::Source {
            exact_label: Some(label),
            ..
        } => label.as_str(),
        MergedCandidate::Source { label, .. } => label.as_str(),
    };
    let profile_category = match candidate {
        MergedCandidate::Source {
            exact_label: Some(label),
            ..
        } => browser.tag_library.profile_categories.get(label),
        MergedCandidate::Profile { label } => browser.tag_library.profile_categories.get(label),
        MergedCandidate::Default(label) => browser.tag_library.profile_categories.get(*label),
        MergedCandidate::Source { .. } => None,
    };
    profile_category
        .and_then(|category| TagCategory::from_id(category.id()))
        .or_else(|| infer_tag_category(label).ok())
        .unwrap_or(TagCategory::Character)
}

fn movable_candidate(candidate: &MergedCandidate) -> bool {
    match candidate {
        MergedCandidate::Default(_) => false,
        MergedCandidate::Profile { label } => {
            is_profile_label(label)
                && infer_tag_category(label).is_ok_and(|category| category.is_user_extensible())
        }
        MergedCandidate::Source {
            exact_label: Some(label),
            ..
        } => {
            is_profile_label(label)
                && infer_tag_category(label).is_ok_and(|category| category.is_user_extensible())
        }
        MergedCandidate::Source {
            exact_label: None, ..
        } => false,
    }
}

fn category_label(candidate: &MergedCandidate) -> Option<&str> {
    match candidate {
        MergedCandidate::Profile { label } => Some(label.as_str()),
        MergedCandidate::Source {
            exact_label: Some(label),
            ..
        } => Some(label.as_str()),
        MergedCandidate::Default(_)
        | MergedCandidate::Source {
            exact_label: None, ..
        } => None,
    }
}

fn profile_definition_status(
    candidate: &MergedCandidate,
    profile_categories: &BTreeMap<String, ProfileTagCategory>,
    lookup_state: ProfileLookupState,
) -> ProfileDefinitionStatus {
    let Some(label) = category_label(candidate) else {
        return match lookup_state {
            ProfileLookupState::Failed => ProfileDefinitionStatus::Failed,
            ProfileLookupState::NotRequested | ProfileLookupState::Pending => {
                ProfileDefinitionStatus::Pending
            }
            ProfileLookupState::Ready => ProfileDefinitionStatus::KnownAbsent,
        };
    };
    if let Some(category) = profile_categories.get(label).copied() {
        return ProfileDefinitionStatus::Known(category);
    }
    match lookup_state {
        ProfileLookupState::Failed => ProfileDefinitionStatus::Failed,
        ProfileLookupState::NotRequested | ProfileLookupState::Pending => {
            ProfileDefinitionStatus::Pending
        }
        ProfileLookupState::Ready => ProfileDefinitionStatus::KnownAbsent,
    }
}

fn definition_removal_target(
    candidate: &MergedCandidate,
    profile_categories: &BTreeMap<String, ProfileTagCategory>,
    lookup_state: ProfileLookupState,
) -> Option<DefinitionRemovalTarget> {
    if let MergedCandidate::Source { tag_id, .. } = candidate
        && *tag_id <= 0
    {
        return None;
    }
    let label = category_label(candidate)?;
    if !is_profile_label(label)
        || !infer_tag_category(label).is_ok_and(|category| category.is_user_extensible())
    {
        return None;
    }
    let ProfileDefinitionStatus::Known(category) =
        profile_definition_status(candidate, profile_categories, lookup_state)
    else {
        return None;
    };
    if category == ProfileTagCategory::PlaybackType {
        return None;
    }
    Some(DefinitionRemovalTarget {
        label: label.to_owned(),
        category,
    })
}

fn is_profile_label(label: &str) -> bool {
    if label.is_empty()
        || label.len() > PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES
        || label.contains('\0')
    {
        return false;
    }
    let bytes = label.as_bytes();
    let mut previous_hyphen = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'_' => previous_hyphen = false,
            b'-' if index > 0 && index + 1 < bytes.len() && !previous_hyphen => {
                previous_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

impl Browser {
    /// Toggle the Tag Editor from the compact add-tag row.
    pub(super) fn toggle_tag_library_from_click(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Browser>,
    ) {
        toggle(self, cx);
    }

    /// Toggle one collapsible Tag Editor category.
    pub(super) fn toggle_tag_library_category(
        &mut self,
        category: usize,
        cx: &mut Context<Browser>,
    ) {
        if let Some(expanded) = self.tag_library.expanded.get_mut(category) {
            *expanded = !*expanded;
            cx.notify();
        }
    }

    /// Capture one bounded tag row as the current category drag source.
    pub(super) fn begin_tag_category_drag(
        &mut self,
        candidate: DragCandidate,
        cx: &mut Context<Browser>,
    ) {
        let current_candidate = self
            .tag_library
            .pager
            .merged_page
            .get(candidate.index)
            .map(MergedCandidate::token);
        if !drag_metadata_is_current(
            candidate.generation,
            candidate.page_epoch,
            &candidate.token,
            candidate.profile_revision,
            self.tag_library.generation,
            self.tag_library.pager.page_epoch,
            current_candidate,
            self.tag_library.profile_revision,
        ) {
            self.tag_library.message =
                Some(String::from("Tag list changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let Some(context) = self
            .tag_library
            .current_target()
            .map(|current| current.context.clone())
        else {
            self.tag_library.message = Some(String::from("Tag library is still loading"));
            cx.notify();
            return;
        };
        if !tags::tag_context_is_current(self, &context) {
            self.tag_library.message =
                Some(String::from("Tag context changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        self.tag_library.dragged = Some(DragTarget {
            label: candidate.label,
            token: candidate.token,
            preview: candidate.preview,
            generation: candidate.generation,
            page_epoch: candidate.page_epoch,
            index: candidate.index,
            context,
            profile_revision: candidate.profile_revision,
        });
        cx.notify();
    }

    /// Apply a category drop through the profile-owned vocabulary writer.
    pub(super) fn drop_tag_category(
        &mut self,
        category: TagCategory,
        candidate: &DragCandidate,
        cx: &mut Context<Browser>,
    ) {
        let Some(stored) = self.tag_library.dragged.as_ref() else {
            return;
        };
        if !drag_payload_matches_target(candidate, stored, cx.entity_id()) {
            self.tag_library.dragged = None;
            self.tag_library.message =
                Some(String::from("Tag drag changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let Some(target) = self.tag_library.dragged.take() else {
            return;
        };
        let current_candidate = self
            .tag_library
            .pager
            .merged_page
            .get(target.index)
            .map(MergedCandidate::token);
        let current_context = self
            .tag_library
            .current_target()
            .map(|current| &current.context);
        if !drag_target_is_current(
            &target,
            self.tag_library.generation,
            self.tag_library.pager.page_epoch,
            current_candidate,
            self.tag_library.profile_revision,
            current_context,
        ) {
            self.tag_library.message =
                Some(String::from("Tag list changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        if !category.is_user_extensible() || target.preview || !is_profile_label(&target.label) {
            self.tag_library.message = Some(String::from(
                "Playback and legacy preview tags keep their inferred category",
            ));
            cx.notify();
            return;
        }
        if !matches!(
            target.token,
            CandidateToken::Profile(_) | CandidateToken::Source(_)
        ) {
            return;
        }
        if !tags::tag_context_is_current(self, &target.context) {
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            return;
        }
        let current_paths = match self.capture_tag_target_paths(1) {
            Ok(paths) => paths,
            Err(error) => {
                self.tag_library.message = Some(error);
                cx.notify();
                return;
            }
        };
        if !drag_target_paths_match(&target, &current_paths) || current_paths.is_empty() {
            self.tag_library.message = Some(String::from("Selection changed; refreshing tags"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let context = target.context;
        if self.tag_library.category_pending.is_some() || self.tag_dictionary.has_pending() {
            self.tag_library.message = Some(String::from("Tag dictionary write is still pending"));
            cx.notify();
            return;
        }
        let Some(revision) = target.profile_revision else {
            self.tag_library.message = Some(String::from("Tag categories are still loading"));
            cx.notify();
            return;
        };
        let Some(global) = cx
            .has_global::<SourceRegistryGlobal>()
            .then(|| cx.global::<SourceRegistryGlobal>())
        else {
            self.tag_library.message = Some(String::from("Profile vocabulary unavailable"));
            cx.notify();
            return;
        };
        let profile_category = ProfileTagCategory::from_id(category.id())
            .expect("core and profile categories share identifiers");
        match global.set_profile_tag_category(revision, target.label.clone(), profile_category) {
            Ok(request) => {
                self.tag_library.category_pending = Some(PendingCategoryWrite {
                    generation: self.tag_library.generation,
                    page_epoch: self.tag_library.pager.page_epoch,
                    context,
                    profile_revision: revision,
                    label: target.label,
                    category: profile_category,
                    request,
                });
                self.tag_library.message = Some(String::from("Saving tag category…"));
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                self.tag_library.message = Some(format!("Could not save tag category: {error}"));
            }
        }
        cx.notify();
    }

    /// Remove one positively-known profile dictionary definition from a fenced row action.
    pub(super) fn remove_tag_library_definition(
        &mut self,
        index: usize,
        token: CandidateToken,
        generation: u64,
        epoch: u64,
        profile_revision: Option<u64>,
        expected: DefinitionRemovalTarget,
        cx: &mut Context<Browser>,
    ) {
        if self.tag_dictionary.has_pending() || self.tag_library.category_pending.is_some() {
            self.tag_library.message = Some(String::from("Tag dictionary write is still pending"));
            cx.notify();
            return;
        }
        if self.tag_library.generation != generation
            || self.tag_library.pager.page_epoch != epoch
            || self
                .tag_library
                .pager
                .merged_page
                .get(index)
                .map(MergedCandidate::token)
                != Some(token.clone())
            || self.tag_library.profile_revision != profile_revision
        {
            self.tag_library.message =
                Some(String::from("Tag list changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let Some(candidate) = self.tag_library.pager.merged_page.get(index) else {
            return;
        };
        let Some(current) = definition_removal_target(
            candidate,
            &self.tag_library.profile_categories,
            self.tag_library.profile_lookup_state,
        ) else {
            self.tag_library.message = Some(String::from(
                "Tag definition is unavailable or cannot be removed",
            ));
            cx.notify();
            return;
        };
        if current != expected || candidate.token() != token {
            self.tag_library.message = Some(String::from("Tag row changed; refresh and try again"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let Some(panel_target) = self.tag_library.current_target().cloned() else {
            self.tag_library.message = Some(String::from("Tag library is still loading"));
            cx.notify();
            return;
        };
        if !tags::tag_context_is_current(self, &panel_target.context) {
            self.tag_library.message = Some(String::from("Tag context changed; refreshing tags"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let current_paths = match self.capture_tag_target_paths(1) {
            Ok(paths) => paths,
            Err(error) => {
                self.tag_library.message = Some(error);
                cx.notify();
                return;
            }
        };
        if current_paths != panel_target.context.paths || current_paths.is_empty() {
            self.tag_library.message = Some(String::from("Selection changed; refreshing tags"));
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let Some(revision) = profile_revision else {
            self.tag_library.message = Some(String::from("Profile tags are still loading"));
            cx.notify();
            return;
        };
        let Some(global) = cx
            .has_global::<SourceRegistryGlobal>()
            .then(|| cx.global::<SourceRegistryGlobal>())
        else {
            self.tag_library.message = Some(String::from("Profile vocabulary unavailable"));
            cx.notify();
            return;
        };
        match self
            .tag_dictionary
            .submit_remove(global, revision, current.label)
        {
            Ok(()) => {
                self.tag_library.message = Some(String::from("Removing tag definition…"));
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                self.tag_library.message =
                    Some(format!("Could not remove tag definition: {error}"));
            }
        }
        cx.notify();
    }

    /// Restore the last successfully removed profile definition at its exact receipt revision.
    pub(super) fn undo_tag_library_definition(&mut self, cx: &mut Context<Browser>) {
        if self.tag_dictionary.has_pending() || self.tag_library.category_pending.is_some() {
            self.tag_library.message = Some(String::from("Tag dictionary write is still pending"));
            cx.notify();
            return;
        }
        let Some((revision, label, category)) = self.tag_dictionary.undo_details() else {
            return;
        };
        let Some(global) = cx
            .has_global::<SourceRegistryGlobal>()
            .then(|| cx.global::<SourceRegistryGlobal>())
        else {
            self.tag_library.message = Some(String::from("Profile vocabulary unavailable"));
            cx.notify();
            return;
        };
        match self
            .tag_dictionary
            .submit_restore(global, revision, label, category)
        {
            Ok(()) => {
                self.tag_library.message = Some(String::from("Restoring tag definition…"));
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                self.tag_library.message =
                    Some(format!("Could not restore tag definition: {error}"));
            }
        }
        cx.notify();
    }

    /// Toggle one source tag assignment after checking its page and source fences.
    pub(super) fn toggle_tag_library_candidate(
        &mut self,
        index: usize,
        candidate: MergedCandidate,
        generation: u64,
        epoch: u64,
        cx: &mut Context<Browser>,
    ) {
        self.tag_library.dragged = None;
        let token = candidate.token();
        if self.tag_library.generation != generation
            || self.tag_library.pager.page_epoch != epoch
            || self
                .tag_library
                .pager
                .merged_page
                .get(index)
                .map(MergedCandidate::token)
                != Some(token)
        {
            self.tag_library.message =
                Some(String::from("Tag list changed; refresh and try again"));
            cx.notify();
            return;
        }
        if self.curation_operation_pending() {
            self.tag_library.message = Some(String::from("Tag operation is still pending"));
            cx.notify();
            return;
        }
        let Some(target) = self.tag_library.current_target().cloned() else {
            return;
        };
        if !tags::tag_context_is_current(self, &target.context) {
            self.tag_library.mark_dirty();
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let current_paths = match self.capture_tag_target_paths(1) {
            Ok(paths) => paths,
            Err(error) => {
                self.tag_library.message = Some(error);
                cx.notify();
                return;
            }
        };
        if current_paths != target.context.paths || current_paths.is_empty() {
            self.tag_library.mark_dirty();
            self.tag_library.message = Some(String::from("Selection changed; refreshing tags"));
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let action = match candidate_action(
            &candidate,
            &self.tag_library.assignment_counts,
            current_paths.len(),
        ) {
            Ok(action) => action,
            Err(error) => {
                self.tag_library.message = Some(String::from(match error {
                    CandidateActionError::InvalidSourceId => "Source tag ID is invalid",
                    CandidateActionError::AssignmentStateUnavailable => {
                        "Tag assignment state is still loading"
                    }
                    CandidateActionError::AssignmentCountOutOfRange => {
                        "Tag assignment state is invalid; refresh and try again"
                    }
                }));
                cx.notify();
                return;
            }
        };
        let result = match action {
            CandidateAction::AddSourceId(tag_id) => {
                self.submit_add_tag_id(tag_id, Some(current_paths), cx)
            }
            CandidateAction::RemoveSourceId(tag_id) => {
                self.submit_remove_tag_id(tag_id, Some(current_paths), cx)
            }
            CandidateAction::AddLabel(label) => {
                self.submit_add_tags(vec![label], Some(current_paths), cx)
            }
        };
        match result {
            TagAdmissionResult::Accepted => {
                self.tag_library.message = Some(String::from("Tag queued"));
                super::tag_chips::invalidate_after_curation(self);
            }
            TagAdmissionResult::NoChange => {
                self.tag_library.message = Some(String::from("No tag updates produced"));
                super::tag_chips::invalidate_after_curation(self);
            }
            TagAdmissionResult::Rejected(error) => self.tag_library.message = Some(error),
        }
        cx.notify();
    }

    /// Navigate to the first Tag Editor page.
    pub(super) fn request_tag_library_first(&mut self, cx: &mut Context<Browser>) {
        if self.tag_library.source_pending.is_some() || self.tag_library.profile_pending.is_some() {
            return;
        }
        self.tag_library.pager.page_history.clear();
        self.tag_library.mark_dirty();
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Navigate to a previous bounded Tag Editor page.
    pub(super) fn request_tag_library_previous(&mut self, cx: &mut Context<Browser>) {
        if self.tag_library.source_pending.is_some() || self.tag_library.profile_pending.is_some() {
            return;
        }
        let Some(checkpoint) = self.tag_library.pager.page_history.pop() else {
            return;
        };
        restore_checkpoint(&mut self.tag_library, checkpoint);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Navigate to the next bounded Tag Editor page.
    pub(super) fn request_tag_library_next(&mut self, cx: &mut Context<Browser>) {
        if self.tag_library.source_pending.is_some()
            || self.tag_library.profile_pending.is_some()
            || !self.tag_library.pager.page_has_next
        {
            return;
        }
        let Some(checkpoint) = self.tag_library.pager.current_checkpoint.clone() else {
            return;
        };
        push_page_history(&mut self.tag_library.pager.page_history, checkpoint);
        let next = self.tag_library.pager.snapshot();
        self.tag_library.pager.current_checkpoint = Some(next);
        self.tag_library.pager.merged_page.clear();
        self.tag_library.pager.selected = None;
        self.tag_library.pager.page_has_next = false;
        self.tag_library.pager.page_epoch = self.tag_library.pager.page_epoch.wrapping_add(1);
        self.tag_library.page_materialized = false;
        self.tag_library.counts_requested = false;
        self.tag_library.categories_requested = false;
        self.tag_library.profile_lookup_state = ProfileLookupState::NotRequested;
        self.tag_library.assignment_counts.clear();
        self.tag_library.profile_categories.clear();
        self.tag_library.counts_target = None;
        self.tag_library.profile_lookup_target = None;
        self.start_source_registry_poll(cx);
        cx.notify();
    }
}

fn restore_checkpoint(state: &mut TagLibraryState, checkpoint: StreamCheckpoint) {
    super::tag_pager::apply_stream_checkpoint(&mut state.pager, &checkpoint);
    state.source_ready = false;
    state.profile_ready = false;
    state.page_materialized = false;
    state.counts_requested = false;
    state.categories_requested = false;
    state.profile_lookup_state = ProfileLookupState::NotRequested;
    state.assignment_counts.clear();
    state.profile_categories.clear();
    state.counts_target = None;
    state.profile_lookup_target = None;
    state.profile_target = None;
    state.source_target = None;
    let Some(target) = state.current_target().cloned() else {
        state.dirty = true;
        return;
    };
    if checkpoint.source_loaded {
        state.source_target = Some(SourcePageTarget {
            generation: target.generation,
            context: target.context.clone(),
            after_cursor: checkpoint.source_after_cursor,
            skip: checkpoint.source_offset,
        });
    } else {
        state.source_ready = true;
    }
    if checkpoint.profile_loaded {
        state.profile_target = Some(ProfilePageTarget {
            generation: target.generation,
            context: target.context,
            expected_revision: checkpoint.profile_revision,
            after_cursor: checkpoint.profile_after_cursor,
            skip: checkpoint.profile_offset,
        });
    } else {
        state.profile_ready = true;
    }
    state.dirty = false;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::super::tag_pager::CandidateToken;
    use super::super::tag_pager::MergedCandidate;
    use super::super::tags::TagSubmissionContext;
    use super::{
        CandidateAction, CandidateActionError, DragTarget, candidate_action,
        context_identity_matches, definition_removal_target, drag_target_is_current,
        drag_target_paths_match, is_profile_label, metadata_target_matches, movable_candidate,
        profile_definition_status,
    };
    use yield_library::sample_sources::ProfileTagCategory;
    use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};

    fn context(revision: u64, paths: &[&str]) -> TagSubmissionContext {
        TagSubmissionContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/source"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 4,
            revision,
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn changed_profile_continuation_releases_target_for_restart() {
        let mut state = super::TagLibraryState::default();
        state.open = true;
        state.generation = 3;
        state.target = Some(super::PanelTarget {
            generation: 3,
            context: context(1, &["one.wav"]),
        });
        state.profile_ready = false;
        state.profile_revision = Some(7);
        assert!(!super::restart_for_profile_revision_change(
            &mut state,
            Some(7),
            7
        ));
        assert!(state.target.is_some());
        assert!(super::restart_for_profile_revision_change(
            &mut state,
            Some(7),
            8
        ));
        assert!(state.dirty);
        assert!(state.target.is_none());
        assert!(state.profile_target.is_none());
        assert_eq!(state.profile_revision, None);
        assert_eq!(state.generation, 4);
    }

    #[test]
    fn profile_lookup_accepts_only_exact_canonical_labels() {
        assert!(is_profile_label("kick"));
        assert!(is_profile_label("one-shot"));
        assert!(!is_profile_label("Kick"));
        assert!(!is_profile_label("legacy tag"));
        assert!(!is_profile_label("one--shot"));
        assert!(!is_profile_label("\0tag"));
    }

    #[test]
    fn legacy_preview_rows_remain_toggleable_but_not_movable() {
        let candidate = MergedCandidate::Source {
            tag_id: 7,
            label: String::from("legacy tag…"),
            exact_label: None,
            revision: 4,
        };
        assert!(!movable_candidate(&candidate));
        assert!(matches!(
            candidate,
            MergedCandidate::Source { tag_id: 7, .. }
        ));
    }

    #[test]
    fn source_action_waits_for_counts_and_preserves_exact_id() {
        let candidate = MergedCandidate::Source {
            tag_id: 17,
            label: String::from("kick"),
            exact_label: Some(String::from("kick")),
            revision: 4,
        };
        let counts = BTreeMap::new();
        assert_eq!(
            candidate_action(&candidate, &counts, 2),
            Err(CandidateActionError::AssignmentStateUnavailable)
        );

        let mut counts = BTreeMap::from([(17, 0)]);
        assert_eq!(
            candidate_action(&candidate, &counts, 2),
            Ok(CandidateAction::AddSourceId(17))
        );
        counts.insert(17, 2);
        assert_eq!(
            candidate_action(&candidate, &counts, 2),
            Ok(CandidateAction::RemoveSourceId(17))
        );
        counts.insert(17, 3);
        assert_eq!(
            candidate_action(&candidate, &counts, 2),
            Err(CandidateActionError::AssignmentCountOutOfRange)
        );
    }

    #[test]
    fn profile_and_default_candidates_use_bounded_label_adds() {
        let counts = BTreeMap::new();
        assert_eq!(
            candidate_action(
                &MergedCandidate::Profile {
                    label: String::from("warm"),
                },
                &counts,
                1,
            ),
            Ok(CandidateAction::AddLabel(String::from("warm")))
        );
        assert_eq!(
            candidate_action(&MergedCandidate::Default("loop"), &counts, 1),
            Ok(CandidateAction::AddLabel(String::from("loop")))
        );
    }

    #[test]
    fn drag_fence_covers_page_identity_and_context_revision() {
        let target_context = context(4, &["kick.wav"]);
        let target = DragTarget {
            label: String::from("warm"),
            token: CandidateToken::Profile(String::from("warm")),
            preview: false,
            generation: 9,
            page_epoch: 12,
            index: 2,
            context: target_context.clone(),
            profile_revision: Some(7),
        };
        assert!(drag_target_is_current(
            &target,
            9,
            12,
            Some(CandidateToken::Profile(String::from("warm"))),
            Some(7),
            Some(&target_context),
        ));
        assert!(!drag_target_is_current(
            &target,
            10,
            12,
            Some(CandidateToken::Profile(String::from("warm"))),
            Some(7),
            Some(&target_context),
        ));
        assert!(!drag_target_is_current(
            &target,
            9,
            13,
            Some(CandidateToken::Profile(String::from("warm"))),
            Some(7),
            Some(&target_context),
        ));
        assert!(!drag_target_is_current(
            &target,
            9,
            12,
            Some(CandidateToken::Profile(String::from("cool"))),
            Some(7),
            Some(&target_context),
        ));
        assert!(!drag_target_is_current(
            &target,
            9,
            12,
            Some(CandidateToken::Profile(String::from("warm"))),
            Some(8),
            Some(&target_context),
        ));
        let changed_context = context(5, &["kick.wav"]);
        assert!(!drag_target_is_current(
            &target,
            9,
            12,
            Some(CandidateToken::Profile(String::from("warm"))),
            Some(7),
            Some(&changed_context),
        ));
        let changed_paths = context(4, &["snare.wav"]);
        assert!(context_identity_matches(&target_context, &changed_paths));
        assert!(!drag_target_paths_match(&target, &changed_paths.paths));
    }

    #[test]
    fn metadata_retry_fence_includes_page_and_profile_revision() {
        let target_context = context(4, &["kick.wav"]);
        assert!(metadata_target_matches(
            9,
            12,
            Some(7),
            &target_context,
            9,
            12,
            Some(7),
            &target_context,
        ));
        assert!(!metadata_target_matches(
            9,
            13,
            Some(7),
            &target_context,
            9,
            12,
            Some(7),
            &target_context,
        ));
        assert!(!metadata_target_matches(
            9,
            12,
            Some(8),
            &target_context,
            9,
            12,
            Some(7),
            &target_context,
        ));
    }

    #[test]
    fn profile_lookup_distinguishes_pending_failed_and_known_absent() {
        let candidate = MergedCandidate::Profile {
            label: String::from("warm"),
        };
        let categories = BTreeMap::new();
        assert_eq!(
            profile_definition_status(
                &candidate,
                &categories,
                super::ProfileLookupState::NotRequested,
            ),
            super::ProfileDefinitionStatus::Pending
        );
        assert_eq!(
            profile_definition_status(&candidate, &categories, super::ProfileLookupState::Pending,),
            super::ProfileDefinitionStatus::Pending
        );
        assert_eq!(
            profile_definition_status(&candidate, &categories, super::ProfileLookupState::Failed,),
            super::ProfileDefinitionStatus::Failed
        );
        assert_eq!(
            profile_definition_status(&candidate, &categories, super::ProfileLookupState::Ready,),
            super::ProfileDefinitionStatus::KnownAbsent
        );
        let categories = BTreeMap::from([(String::from("warm"), ProfileTagCategory::Character)]);
        assert_eq!(
            profile_definition_status(&candidate, &categories, super::ProfileLookupState::Pending,),
            super::ProfileDefinitionStatus::Known(ProfileTagCategory::Character)
        );
    }

    #[test]
    fn definition_removal_requires_a_loaded_canonical_extensible_definition() {
        let categories = BTreeMap::from([(String::from("warm"), ProfileTagCategory::Character)]);
        let profile = MergedCandidate::Profile {
            label: String::from("warm"),
        };
        assert_eq!(
            definition_removal_target(&profile, &categories, super::ProfileLookupState::Pending,),
            Some(super::DefinitionRemovalTarget {
                label: String::from("warm"),
                category: ProfileTagCategory::Character,
            })
        );
        let source = MergedCandidate::Source {
            tag_id: 9,
            label: String::from("warm"),
            exact_label: Some(String::from("warm")),
            revision: 4,
        };
        assert!(
            definition_removal_target(&source, &categories, super::ProfileLookupState::Ready,)
                .is_some()
        );
        let preview = MergedCandidate::Source {
            tag_id: 9,
            label: String::from("warm…"),
            exact_label: None,
            revision: 4,
        };
        assert!(
            definition_removal_target(&preview, &categories, super::ProfileLookupState::Ready,)
                .is_none()
        );
        let invalid_id = MergedCandidate::Source {
            tag_id: 0,
            label: String::from("warm"),
            exact_label: Some(String::from("warm")),
            revision: 4,
        };
        assert!(
            definition_removal_target(&invalid_id, &categories, super::ProfileLookupState::Ready,)
                .is_none()
        );
        let locked = MergedCandidate::Profile {
            label: String::from("one-shot"),
        };
        let locked_categories =
            BTreeMap::from([(String::from("one-shot"), ProfileTagCategory::PlaybackType)]);
        assert!(
            definition_removal_target(
                &locked,
                &locked_categories,
                super::ProfileLookupState::Ready,
            )
            .is_none()
        );
        let noncanonical = MergedCandidate::Profile {
            label: String::from("legacy tag"),
        };
        let noncanonical_categories =
            BTreeMap::from([(String::from("legacy tag"), ProfileTagCategory::Character)]);
        assert!(
            definition_removal_target(
                &noncanonical,
                &noncanonical_categories,
                super::ProfileLookupState::Ready,
            )
            .is_none()
        );
        assert!(
            definition_removal_target(
                &profile,
                &BTreeMap::new(),
                super::ProfileLookupState::Pending,
            )
            .is_none()
        );
    }
}
