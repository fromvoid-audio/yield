#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded add-tag completion over source and profile vocabularies.

use std::sync::mpsc::TryRecvError;

use gpui::{ClickEvent, Context, ScrollHandle, div, prelude::*, px};
use wavecrate_core::{DEFAULT_METADATA_TAGS, parse_tag_input};
use wavecrate_design::Theme;
use wavecrate_library::sample_sources::{
    PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE, ProfileTagVocabularyEntry, ProfileTagVocabularyPage,
    ProfileTagVocabularyPageCursor, SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
    SOURCE_TAG_PAGE_MAX_PREFIX_BYTES, SourceTagPageCursor, SourceTagPageEntry,
};
use wavecrate_session::{
    CatalogCommand, CatalogEvent, CatalogSubmitError, CatalogTagPageQuery, VocabularyRequest,
    VocabularySubmitError,
};

use crate::theme::{color, transparent};

use super::catalog;
use super::curation::TagAdmissionResult;
use super::source_registry::SourceRegistryGlobal;
use super::{Browser, tags};

const MAX_CURSOR_HISTORY: usize = 32;
const MAX_SEEN_EXACT: usize = 128;
const MERGED_PAGE_SIZE: usize = 6;

/// Browser-owned state for one bounded add-tag completion query.
pub(super) struct TagCompletionState {
    draft_generation: u64,
    desired: Option<QueryIntent>,
    source_pending: Option<PendingSourcePage>,
    source_target: Option<SourcePageTarget>,
    source_ready: bool,
    source_page: Option<SourcePageWindow>,
    profile_pending: Option<PendingProfilePage>,
    profile_target: Option<ProfilePageTarget>,
    profile_ready: bool,
    profile_page: Option<ProfilePageWindow>,
    default_offset: usize,
    merged_page: Vec<MergedCandidate>,
    current_checkpoint: Option<StreamCheckpoint>,
    page_history: Vec<StreamCheckpoint>,
    seen_exact: Vec<String>,
    page_epoch: u64,
    page_has_next: bool,
    selected: Option<usize>,
    message: Option<String>,
    suppress_next_input: bool,
    pub(super) scroll_handle: ScrollHandle,
}

impl Default for TagCompletionState {
    fn default() -> Self {
        Self {
            draft_generation: 0,
            desired: None,
            source_pending: None,
            source_target: None,
            source_ready: false,
            source_page: None,
            profile_pending: None,
            profile_target: None,
            profile_ready: false,
            profile_page: None,
            default_offset: 0,
            merged_page: Vec::new(),
            current_checkpoint: None,
            page_history: Vec::new(),
            seen_exact: Vec::new(),
            page_epoch: 0,
            page_has_next: false,
            selected: None,
            message: None,
            suppress_next_input: false,
            scroll_handle: ScrollHandle::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueryIntent {
    draft_generation: u64,
    prefix: String,
    context: tags::TagSubmissionContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourcePageTarget {
    query: QueryIntent,
    after_cursor: Option<SourceTagPageCursor>,
    skip: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProfilePageTarget {
    query: QueryIntent,
    expected_revision: Option<u64>,
    after_cursor: Option<ProfileTagVocabularyPageCursor>,
    skip: usize,
}

struct PendingSourcePage {
    request_id: u64,
    target: SourcePageTarget,
}

struct PendingProfilePage {
    target: ProfilePageTarget,
    request: VocabularyRequest<ProfileTagVocabularyPage>,
}

struct SourcePageWindow {
    revision: u64,
    entries: Vec<SourceTagPageEntry>,
    next_cursor: Option<SourceTagPageCursor>,
    page_start: Option<SourceTagPageCursor>,
    offset: usize,
}

struct ProfilePageWindow {
    revision: u64,
    entries: Vec<ProfileTagVocabularyEntry>,
    next_cursor: Option<ProfileTagVocabularyPageCursor>,
    page_start: Option<ProfileTagVocabularyPageCursor>,
    offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamCheckpoint {
    source_loaded: bool,
    source_after_cursor: Option<SourceTagPageCursor>,
    source_offset: usize,
    profile_loaded: bool,
    profile_revision: Option<u64>,
    profile_after_cursor: Option<ProfileTagVocabularyPageCursor>,
    profile_offset: usize,
    default_offset: usize,
    seen_exact: Vec<String>,
}

impl StreamCheckpoint {
    fn initial() -> Self {
        Self {
            source_loaded: true,
            source_after_cursor: None,
            source_offset: 0,
            profile_loaded: true,
            profile_revision: None,
            profile_after_cursor: None,
            profile_offset: 0,
            default_offset: 0,
            seen_exact: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MergedCandidate {
    Default(&'static str),
    Profile {
        label: String,
    },
    Source {
        tag_id: i64,
        label: String,
        exact_label: Option<String>,
        revision: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CandidateToken {
    Default(&'static str),
    Profile(String),
    Source(i64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Default,
    Profile,
    Source,
}

#[derive(Clone, Copy)]
enum SelectionDirection {
    Previous,
    Next,
}

impl TagCompletionState {
    fn bump_generation(&mut self) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
    }

    fn invalidate_menu(&mut self) {
        self.merged_page.clear();
        self.selected = None;
        self.page_has_next = false;
        self.page_epoch = self.page_epoch.wrapping_add(1);
    }

    fn clear_pages(&mut self) {
        self.source_page = None;
        self.profile_page = None;
        self.default_offset = 0;
        self.current_checkpoint = None;
        self.page_history.clear();
        self.seen_exact.clear();
        self.invalidate_menu();
    }

    fn clear_query(&mut self, drop_pending: bool) {
        self.desired = None;
        self.source_target = None;
        self.profile_target = None;
        self.source_ready = false;
        self.profile_ready = false;
        self.clear_pages();
        self.message = None;
        if drop_pending {
            self.source_pending = None;
            self.profile_pending = None;
        }
    }

    fn set_query(&mut self, desired: Option<QueryIntent>) {
        self.desired = desired.clone();
        self.source_target = desired.clone().map(|query| SourcePageTarget {
            query,
            after_cursor: None,
            skip: 0,
        });
        self.profile_target = desired.map(|query| ProfilePageTarget {
            query,
            expected_revision: None,
            after_cursor: None,
            skip: 0,
        });
        self.source_ready = false;
        self.profile_ready = false;
        self.clear_pages();
        if self.desired.is_some() {
            self.current_checkpoint = Some(StreamCheckpoint::initial());
        }
        self.message = None;
    }

    fn query_is_current(&self, query: &QueryIntent) -> bool {
        self.desired.as_ref() == Some(query)
    }

    fn source_target_is_current(&self, target: &SourcePageTarget) -> bool {
        self.source_target.as_ref() == Some(target) && self.query_is_current(&target.query)
    }

    fn profile_target_is_current(&self, target: &ProfilePageTarget) -> bool {
        self.profile_target.as_ref() == Some(target) && self.query_is_current(&target.query)
    }

    fn snapshot(&self) -> StreamCheckpoint {
        StreamCheckpoint {
            source_loaded: self.source_page.is_some(),
            source_after_cursor: self.source_page.as_ref().and_then(|page| page.page_start),
            source_offset: self
                .source_page
                .as_ref()
                .map(|page| page.offset)
                .unwrap_or_default(),
            profile_loaded: self.profile_page.is_some(),
            profile_revision: self.profile_page.as_ref().map(|page| page.revision),
            profile_after_cursor: self
                .profile_page
                .as_ref()
                .and_then(|page| page.page_start.clone()),
            profile_offset: self
                .profile_page
                .as_ref()
                .map(|page| page.offset)
                .unwrap_or_default(),
            default_offset: self.default_offset,
            seen_exact: self.seen_exact.clone(),
        }
    }

    fn candidate_count(&self) -> usize {
        self.merged_page.len()
    }

    fn candidate(&self, index: usize) -> Option<MergedCandidate> {
        self.merged_page.get(index).cloned()
    }
}

/// Advance completion state after a native tag-input edit.
pub(super) fn on_changed(browser: &mut Browser, input: &str, cx: &mut Context<Browser>) {
    if browser.tag_completion.suppress_next_input {
        browser.tag_completion.suppress_next_input = false;
        browser.tag_completion.bump_generation();
        browser.tag_completion.clear_query(false);
        cx.notify();
        return;
    }

    let next_generation = browser.tag_completion.draft_generation.wrapping_add(1);
    let desired = normalized_completion_prefix(input).and_then(|prefix| {
        tags::capture_tag_context(browser, Vec::new()).map(|context| QueryIntent {
            draft_generation: next_generation,
            prefix,
            context,
        })
    });
    browser.tag_completion.draft_generation = next_generation;
    browser.tag_completion.set_query(desired);
    clear_completion_ghost(browser, cx);
    if browser.tag_completion.desired.is_some() {
        browser.start_source_registry_poll(cx);
    }
    cx.notify();
}

/// Show a bounded input rejection while retaining the last accepted completion query.
pub(super) fn on_rejected(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_completion.message = Some(String::from("Tag completion input is too large"));
    cx.notify();
}

/// Return whether a completion request or admitted retry still needs polling.
pub(super) fn has_pending(browser: &Browser) -> bool {
    browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
        || browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
}

/// Cancel completion when the visible editor is dismissed.
pub(super) fn cancel_for_editor(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_completion.bump_generation();
    browser.tag_completion.clear_query(true);
    browser.tag_completion.suppress_next_input = false;
    clear_completion_ghost(browser, cx);
}

/// Cancel completion across a source lifecycle change.
pub(super) fn cancel_for_source_change(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_completion.bump_generation();
    browser.tag_completion.clear_query(true);
    browser.tag_completion.suppress_next_input = false;
    clear_completion_ghost(browser, cx);
}

/// Consume a source vocabulary page for the matching completion request.
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
    let Some(pending) = browser.tag_completion.source_pending.as_ref() else {
        return false;
    };
    if pending.request_id != request_id
        || pending.target.query.context.source_id != *source_id
        || pending.target.query.context.lifecycle_generation != lifecycle_generation
    {
        return false;
    }
    let pending = browser
        .tag_completion
        .source_pending
        .take()
        .expect("completion source request was checked");
    let current = browser
        .tag_completion
        .source_target_is_current(&pending.target);
    match event {
        CatalogEvent::TagPageReady {
            root_identity,
            revision,
            page,
            ..
        } if current
            && tags::source_page_reply_is_coherent(
                &pending.target.query.context,
                source_id,
                lifecycle_generation,
                root_identity,
                *revision,
                page.revision,
            )
            && page.entries.len() <= SOURCE_TAG_PAGE_MAX_PAGE_SIZE =>
        {
            install_source_page(
                &mut browser.tag_completion,
                wavecrate_library::sample_sources::SourceTagPage {
                    revision: page.revision,
                    entries: page.entries.clone(),
                    next_cursor: page.next_cursor,
                },
                pending.target.after_cursor,
                pending.target.skip,
            );
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.message = None;
            merge_available_into_page(browser, cx);
        }
        CatalogEvent::TagPageFailed {
            expected_revision,
            observed_revision,
            error,
            ..
        } if current && *expected_revision == pending.target.query.context.revision => {
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.source_page = None;
            browser.tag_completion.message = Some(
                if observed_revision
                    .is_some_and(|revision| revision != pending.target.query.context.revision)
                {
                    String::from("Source tag suggestions are stale; edit the tag to retry")
                } else {
                    format!("Could not load source tag suggestions: {error}")
                },
            );
            merge_available_into_page(browser, cx);
        }
        _ => {
            if current {
                browser.tag_completion.source_target = None;
                browser.tag_completion.source_ready = true;
                browser.tag_completion.source_page = None;
                browser.tag_completion.message = Some(String::from(
                    "Source tag suggestions no longer match this source revision",
                ));
                merge_available_into_page(browser, cx);
            }
        }
    }
    true
}

/// Poll both bounded completion lanes without starting an idle timer of their own.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let mut changed = false;
    if browser.tag_completion.source_pending.is_none()
        && let Some(target) = browser.tag_completion.source_target.clone()
    {
        changed |= submit_source_page(browser, target, cx);
    }
    if browser.tag_completion.profile_pending.is_none()
        && let Some(target) = browser.tag_completion.profile_target.clone()
    {
        changed |= submit_profile_page(browser, target, cx);
    }
    changed |= poll_profile_page(browser, cx);
    changed
}

fn submit_source_page(
    browser: &mut Browser,
    target: SourcePageTarget,
    cx: &mut Context<Browser>,
) -> bool {
    if !browser.tag_completion.source_target_is_current(&target) {
        return false;
    }
    let request_id = browser.next_tag_request_id();
    let command = CatalogCommand::RequestTagPage {
        source_id: target.query.context.source_id.clone(),
        lifecycle_generation: target.query.context.lifecycle_generation,
        expected_revision: target.query.context.revision,
        request_id,
        query: CatalogTagPageQuery::VocabularyPrefix {
            prefix: target.query.prefix.clone(),
        },
        after_cursor: target.after_cursor,
        page_limit: SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
    };
    match catalog::submit_catalog_command(command, cx) {
        Ok(()) => {
            browser.tag_completion.source_pending = Some(PendingSourcePage { request_id, target });
            true
        }
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
            browser.tag_completion.message =
                Some(String::from("Tag suggestions are waiting for the catalog…"));
            true
        }
        Err(CatalogSubmitError::Unavailable(error)) => {
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.message = Some(format!("Tag suggestions unavailable: {error}"));
            merge_available_into_page(browser, cx);
            true
        }
        Err(CatalogSubmitError::Closed) => {
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.message = Some(String::from("Source catalog closed"));
            merge_available_into_page(browser, cx);
            true
        }
        Err(CatalogSubmitError::InvalidTagPageRequest(error)) => {
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.message =
                Some(format!("Could not load tag suggestions: {error}"));
            merge_available_into_page(browser, cx);
            true
        }
        Err(
            CatalogSubmitError::InvalidWavMetadataRequest(_)
            | CatalogSubmitError::InvalidCurationRequest(_)
            | CatalogSubmitError::InvalidTagFilterRequest(_),
        ) => {
            browser.tag_completion.source_target = None;
            browser.tag_completion.source_ready = true;
            browser.tag_completion.message = Some(String::from(
                "Could not load tag suggestions: invalid catalog request",
            ));
            merge_available_into_page(browser, cx);
            true
        }
    }
}

fn submit_profile_page(
    browser: &mut Browser,
    target: ProfilePageTarget,
    cx: &mut Context<Browser>,
) -> bool {
    if !browser.tag_completion.profile_target_is_current(&target) {
        return false;
    }
    let Some(global) = cx
        .has_global::<SourceRegistryGlobal>()
        .then(|| cx.global::<SourceRegistryGlobal>())
    else {
        browser.tag_completion.profile_target = None;
        browser.tag_completion.profile_ready = true;
        browser.tag_completion.message = Some(String::from("Tag suggestions are unavailable"));
        merge_available_into_page(browser, cx);
        return true;
    };
    match global.profile_tag_vocabulary_page(
        target.expected_revision,
        target.query.prefix.clone(),
        target.after_cursor.clone(),
        PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE,
    ) {
        Ok(request) => {
            browser.tag_completion.profile_pending = Some(PendingProfilePage { target, request });
            true
        }
        Err(VocabularySubmitError::Full | VocabularySubmitError::Initializing) => {
            browser.tag_completion.message = Some(String::from(
                "Tag suggestions are waiting for the vocabulary…",
            ));
            true
        }
        Err(VocabularySubmitError::Unavailable(error)) => {
            browser.tag_completion.profile_target = None;
            browser.tag_completion.profile_ready = true;
            browser.tag_completion.message = Some(format!("Tag suggestions unavailable: {error}"));
            merge_available_into_page(browser, cx);
            true
        }
        Err(VocabularySubmitError::Closed) => {
            browser.tag_completion.profile_target = None;
            browser.tag_completion.profile_ready = true;
            browser.tag_completion.message = Some(String::from("Tag vocabulary closed"));
            merge_available_into_page(browser, cx);
            true
        }
        Err(VocabularySubmitError::Invalid(error)) => {
            browser.tag_completion.profile_target = None;
            browser.tag_completion.profile_ready = true;
            browser.tag_completion.message =
                Some(format!("Could not load tag suggestions: {error}"));
            merge_available_into_page(browser, cx);
            true
        }
    }
}

fn poll_profile_page(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(mut pending) = browser.tag_completion.profile_pending.take() else {
        return false;
    };
    let result = match pending.request.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            browser.tag_completion.profile_pending = Some(pending);
            return false;
        }
        Err(TryRecvError::Disconnected) => {
            if browser
                .tag_completion
                .profile_target_is_current(&pending.target)
            {
                browser.tag_completion.profile_target = None;
                browser.tag_completion.profile_ready = true;
                browser.tag_completion.message =
                    Some(String::from("Tag vocabulary request disconnected"));
                merge_available_into_page(browser, cx);
            }
            return true;
        }
    };
    match result {
        Ok(page) => apply_profile_page(browser, pending.target, page, cx),
        Err(error) => {
            if browser
                .tag_completion
                .profile_target_is_current(&pending.target)
            {
                browser.tag_completion.profile_target = None;
                browser.tag_completion.profile_ready = true;
                browser.tag_completion.message =
                    Some(format!("Could not load tag suggestions: {error}"));
                merge_available_into_page(browser, cx);
            }
            true
        }
    }
}

fn apply_profile_page(
    browser: &mut Browser,
    target: ProfilePageTarget,
    page: ProfileTagVocabularyPage,
    cx: &mut Context<Browser>,
) -> bool {
    if !browser.tag_completion.profile_target_is_current(&target) {
        return true;
    }
    if page.entries.len() > PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE
        || target
            .expected_revision
            .is_some_and(|revision| revision != page.revision)
    {
        browser.tag_completion.profile_target = None;
        browser.tag_completion.profile_ready = true;
        browser.tag_completion.profile_page = None;
        browser.tag_completion.message = Some(String::from(
            "Tag suggestions no longer match this vocabulary revision",
        ));
        merge_available_into_page(browser, cx);
        return true;
    }
    let entry_count = page.entries.len();
    browser.tag_completion.profile_page = Some(ProfilePageWindow {
        revision: page.revision,
        entries: page.entries,
        next_cursor: page.next_cursor,
        page_start: target.after_cursor,
        offset: target.skip.min(entry_count),
    });
    browser.tag_completion.profile_target = None;
    browser.tag_completion.profile_ready = true;
    browser.tag_completion.message = None;
    merge_available_into_page(browser, cx);
    true
}

fn queue_source_page(
    browser: &mut Browser,
    after_cursor: SourceTagPageCursor,
    cx: &mut Context<Browser>,
) {
    if browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
    {
        return;
    }
    let Some(query) = browser.tag_completion.desired.clone() else {
        return;
    };
    browser.tag_completion.source_target = Some(SourcePageTarget {
        query,
        after_cursor: Some(after_cursor),
        skip: 0,
    });
    browser.start_source_registry_poll(cx);
}

fn queue_profile_page(
    browser: &mut Browser,
    after_cursor: ProfileTagVocabularyPageCursor,
    revision: u64,
    cx: &mut Context<Browser>,
) {
    if browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
    {
        return;
    }
    let Some(query) = browser.tag_completion.desired.clone() else {
        return;
    };
    browser.tag_completion.profile_target = Some(ProfilePageTarget {
        query,
        expected_revision: Some(revision),
        after_cursor: Some(after_cursor),
        skip: 0,
    });
    browser.start_source_registry_poll(cx);
}

fn push_page_history(history: &mut Vec<StreamCheckpoint>, checkpoint: StreamCheckpoint) {
    if history.len() >= MAX_CURSOR_HISTORY {
        history.remove(0);
    }
    history.push(checkpoint);
}

fn merge_available_into_page(browser: &mut Browser, cx: &mut Context<Browser>) {
    let Some(query) = browser.tag_completion.desired.clone() else {
        return;
    };
    if !browser.tag_completion.source_ready
        || !browser.tag_completion.profile_ready
        || browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
        || browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
    {
        return;
    }

    let merge = merge_ready_page(&mut browser.tag_completion, &query.prefix);
    if let Some(cursor) = merge.source_after {
        queue_source_page(browser, cursor, cx);
    }
    if let Some(cursor) = merge.profile_after {
        let revision = browser
            .tag_completion
            .profile_page
            .as_ref()
            .map(|page| page.revision);
        if let Some(revision) = revision {
            queue_profile_page(browser, cursor, revision, cx);
        }
    }
    sync_completion_ghost(browser, cx, &query.prefix);
}

struct MergePageResult {
    source_after: Option<SourceTagPageCursor>,
    profile_after: Option<ProfileTagVocabularyPageCursor>,
}

fn merge_ready_page(state: &mut TagCompletionState, prefix: &str) -> MergePageResult {
    let mut source_after = None;
    let mut profile_after = None;
    let mut changed = false;
    while state.merged_page.len() < MERGED_PAGE_SIZE {
        if let Some(cursor) = source_next_cursor(state) {
            source_after = Some(cursor);
            break;
        }
        if let Some(cursor) = profile_next_cursor(state) {
            profile_after = Some(cursor);
            break;
        }
        let Some(lane) = choose_lane(state, prefix) else {
            break;
        };
        let exact = lane_exact_key(state, lane, prefix);
        let candidate = lane_candidate(state, lane, prefix);
        advance_lane(state, lane);
        let duplicate = exact
            .as_deref()
            .is_some_and(|key| state.seen_exact.iter().any(|seen| seen == key));
        if duplicate {
            continue;
        }
        if let Some(key) = exact {
            if state.seen_exact.len() >= MAX_SEEN_EXACT {
                state.seen_exact.remove(0);
            }
            state.seen_exact.push(key);
        }
        let Some(candidate) = candidate else {
            break;
        };
        state.merged_page.push(candidate);
        changed = true;
    }

    state.page_has_next =
        state.merged_page.len() >= MERGED_PAGE_SIZE && has_unconsumed_candidate(state, prefix);
    if changed {
        state.selected = None;
        state.page_epoch = state.page_epoch.wrapping_add(1);
    }
    MergePageResult {
        source_after,
        profile_after,
    }
}

fn has_unconsumed_candidate(state: &TagCompletionState, prefix: &str) -> bool {
    default_head(prefix, state.default_offset).is_some()
        || profile_head(state, prefix).is_some()
        || source_head(state, prefix).is_some()
        || source_next_cursor(state).is_some()
        || profile_next_cursor(state).is_some()
}

fn source_next_cursor(state: &TagCompletionState) -> Option<SourceTagPageCursor> {
    state.source_page.as_ref().and_then(|page| {
        (page.offset >= page.entries.len())
            .then_some(page.next_cursor)
            .flatten()
    })
}

fn profile_next_cursor(state: &TagCompletionState) -> Option<ProfileTagVocabularyPageCursor> {
    state.profile_page.as_ref().and_then(|page| {
        (page.offset >= page.entries.len())
            .then(|| page.next_cursor.clone())
            .flatten()
    })
}

fn default_head(prefix: &str, offset: usize) -> Option<&'static str> {
    DEFAULT_METADATA_TAGS
        .into_iter()
        .filter(|label| label.starts_with(prefix))
        .nth(offset)
}

fn profile_head<'a>(
    state: &'a TagCompletionState,
    prefix: &str,
) -> Option<&'a ProfileTagVocabularyEntry> {
    state.profile_page.as_ref().and_then(|page| {
        page.entries
            .get(page.offset)
            .filter(|entry| entry.label.starts_with(prefix))
    })
}

fn source_head<'a>(state: &'a TagCompletionState, prefix: &str) -> Option<&'a SourceTagPageEntry> {
    state.source_page.as_ref().and_then(|page| {
        page.entries
            .get(page.offset)
            .filter(|entry| entry.is_preview || entry.normalized_text.starts_with(prefix))
    })
}

fn lane_key<'a>(state: &'a TagCompletionState, lane: Lane, prefix: &str) -> Option<&'a str> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset),
        Lane::Profile => profile_head(state, prefix).map(|entry| entry.label.as_str()),
        Lane::Source => source_head(state, prefix).map(|entry| entry.normalized_text.as_str()),
    }
}

fn lane_source_id(state: &TagCompletionState, lane: Lane, prefix: &str) -> i64 {
    match lane {
        Lane::Source => source_head(state, prefix)
            .map(|entry| entry.id)
            .unwrap_or_default(),
        Lane::Default | Lane::Profile => 0,
    }
}

fn lane_priority(lane: Lane) -> u8 {
    match lane {
        Lane::Source => 0,
        Lane::Profile => 1,
        Lane::Default => 2,
    }
}

fn compare_lanes(
    state: &TagCompletionState,
    left: Lane,
    right: Lane,
    prefix: &str,
) -> std::cmp::Ordering {
    lane_key(state, left, prefix)
        .cmp(&lane_key(state, right, prefix))
        .then_with(|| lane_priority(left).cmp(&lane_priority(right)))
        .then_with(|| {
            lane_source_id(state, left, prefix).cmp(&lane_source_id(state, right, prefix))
        })
}

fn choose_lane(state: &TagCompletionState, prefix: &str) -> Option<Lane> {
    [Lane::Default, Lane::Profile, Lane::Source]
        .into_iter()
        .filter(|lane| lane_key(state, *lane, prefix).is_some())
        .min_by(|left, right| compare_lanes(state, *left, *right, prefix))
}

fn lane_exact_key(state: &TagCompletionState, lane: Lane, prefix: &str) -> Option<String> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset).map(String::from),
        Lane::Profile => profile_head(state, prefix).map(|entry| entry.label.clone()),
        Lane::Source => source_head(state, prefix)
            .filter(|entry| !entry.is_preview)
            .map(|entry| entry.normalized_text.clone()),
    }
}

fn lane_candidate(state: &TagCompletionState, lane: Lane, prefix: &str) -> Option<MergedCandidate> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset).map(MergedCandidate::Default),
        Lane::Profile => profile_head(state, prefix).map(|entry| MergedCandidate::Profile {
            label: entry.label.clone(),
        }),
        Lane::Source => source_head(state, prefix).map(|entry| MergedCandidate::Source {
            tag_id: entry.id,
            label: source_display_label(entry),
            exact_label: (!entry.is_preview).then(|| entry.normalized_text.clone()),
            revision: state
                .source_page
                .as_ref()
                .map(|page| page.revision)
                .unwrap_or_default(),
        }),
    }
}

fn advance_lane(state: &mut TagCompletionState, lane: Lane) {
    match lane {
        Lane::Default => state.default_offset = state.default_offset.saturating_add(1),
        Lane::Profile => {
            if let Some(page) = state.profile_page.as_mut() {
                page.offset = page.offset.saturating_add(1);
            }
        }
        Lane::Source => {
            if let Some(page) = state.source_page.as_mut() {
                page.offset = page.offset.saturating_add(1);
            }
        }
    }
}

fn source_display_label(entry: &SourceTagPageEntry) -> String {
    if entry.is_preview && !entry.display_label.ends_with('…') {
        format!("{}…", entry.display_label)
    } else {
        entry.display_label.clone()
    }
}

fn install_source_page(
    state: &mut TagCompletionState,
    page: wavecrate_library::sample_sources::SourceTagPage,
    page_start: Option<SourceTagPageCursor>,
    skip: usize,
) {
    let entry_count = page.entries.len();
    state.source_page = Some(SourcePageWindow {
        revision: page.revision,
        entries: page.entries,
        next_cursor: page.next_cursor,
        page_start,
        offset: skip.min(entry_count),
    });
}

fn sync_completion_ghost(browser: &mut Browser, cx: &mut Context<Browser>, prefix: &str) {
    let ghost = browser
        .tag_completion
        .merged_page
        .first()
        .and_then(|candidate| candidate.ghost_suffix(prefix))
        .unwrap_or_default();
    browser
        .tag_input
        .update(cx, |input, cx| input.set_ghost(ghost, cx));
}

fn clear_completion_ghost(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser
        .tag_input
        .update(cx, |input, cx| input.set_ghost("", cx));
}

fn restore_checkpoint(
    browser: &mut Browser,
    checkpoint: StreamCheckpoint,
    cx: &mut Context<Browser>,
) {
    let Some(query) = browser.tag_completion.desired.clone() else {
        return;
    };
    clear_completion_ghost(browser, cx);
    apply_stream_checkpoint(&mut browser.tag_completion, &checkpoint);

    if checkpoint.source_loaded {
        browser.tag_completion.source_ready = false;
        browser.tag_completion.source_target = Some(SourcePageTarget {
            query: query.clone(),
            after_cursor: checkpoint.source_after_cursor,
            skip: checkpoint.source_offset,
        });
    } else {
        browser.tag_completion.source_ready = true;
        browser.tag_completion.source_target = None;
    }
    if checkpoint.profile_loaded {
        browser.tag_completion.profile_ready = false;
        browser.tag_completion.profile_target = Some(ProfilePageTarget {
            query,
            expected_revision: checkpoint.profile_revision,
            after_cursor: checkpoint.profile_after_cursor,
            skip: checkpoint.profile_offset,
        });
    } else {
        browser.tag_completion.profile_ready = true;
        browser.tag_completion.profile_target = None;
    }
    browser.start_source_registry_poll(cx);
}

fn apply_stream_checkpoint(state: &mut TagCompletionState, checkpoint: &StreamCheckpoint) {
    state.source_page = None;
    state.profile_page = None;
    state.source_ready = !checkpoint.source_loaded;
    state.profile_ready = !checkpoint.profile_loaded;
    state.default_offset = checkpoint.default_offset;
    state.seen_exact = checkpoint.seen_exact.clone();
    state.merged_page.clear();
    state.selected = None;
    state.page_has_next = false;
    state.page_epoch = state.page_epoch.wrapping_add(1);
    state.current_checkpoint = Some(checkpoint.clone());
}

fn request_next(browser: &mut Browser, epoch: u64, cx: &mut Context<Browser>) {
    if epoch != browser.tag_completion.page_epoch
        || browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
        || browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
        || !browser.tag_completion.page_has_next
    {
        return;
    }
    let Some(checkpoint) = browser.tag_completion.current_checkpoint.clone() else {
        return;
    };
    push_page_history(&mut browser.tag_completion.page_history, checkpoint);
    let next_checkpoint = browser.tag_completion.snapshot();
    browser.tag_completion.current_checkpoint = Some(next_checkpoint);
    browser.tag_completion.merged_page.clear();
    browser.tag_completion.selected = None;
    browser.tag_completion.page_has_next = false;
    browser.tag_completion.page_epoch = browser.tag_completion.page_epoch.wrapping_add(1);
    merge_available_into_page(browser, cx);
    browser.start_source_registry_poll(cx);
    cx.notify();
}

fn request_previous(browser: &mut Browser, epoch: u64, cx: &mut Context<Browser>) {
    if epoch != browser.tag_completion.page_epoch
        || browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
        || browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
    {
        return;
    }
    let Some(checkpoint) = browser.tag_completion.page_history.pop() else {
        return;
    };
    restore_checkpoint(browser, checkpoint, cx);
    cx.notify();
}

fn request_first(browser: &mut Browser, epoch: u64, cx: &mut Context<Browser>) {
    if epoch != browser.tag_completion.page_epoch
        || browser.tag_completion.page_history.is_empty()
        || browser.tag_completion.source_pending.is_some()
        || browser.tag_completion.source_target.is_some()
        || browser.tag_completion.profile_pending.is_some()
        || browser.tag_completion.profile_target.is_some()
    {
        return;
    }
    browser.tag_completion.page_history.clear();
    restore_checkpoint(browser, StreamCheckpoint::initial(), cx);
    cx.notify();
}

/// Intercept completion keyboard navigation while the add-tag input is focused.
pub(super) fn handle_focused_key_down(
    browser: &mut Browser,
    key: &str,
    is_held: bool,
    is_composing: bool,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
    cx: &mut Context<Browser>,
) -> bool {
    if tags::category_picker_active(browser) {
        return false;
    }
    if is_held {
        return true;
    }
    if is_composing || secondary || shift || alt || function {
        return false;
    }
    let Some(query) = browser.tag_completion.desired.as_ref() else {
        return false;
    };
    let query_generation = query.draft_generation;
    let count = browser.tag_completion.candidate_count();
    match key {
        "up" | "arrowup" => {
            if count > 0 {
                browser.tag_completion.selected = next_selection(
                    browser.tag_completion.selected,
                    count,
                    SelectionDirection::Previous,
                );
                browser.tag_completion.message = None;
                cx.notify();
            }
            true
        }
        "down" | "arrowdown" => {
            if count > 0 {
                browser.tag_completion.selected = next_selection(
                    browser.tag_completion.selected,
                    count,
                    SelectionDirection::Next,
                );
                browser.tag_completion.message = None;
                cx.notify();
            }
            true
        }
        "enter" if browser.tag_completion.selected.is_some() => {
            let index = browser.tag_completion.selected.unwrap_or_default();
            let token = browser
                .tag_completion
                .candidate(index)
                .map(|candidate| candidate.token());
            let Some(token) = token else {
                return false;
            };
            let epoch = browser.tag_completion.page_epoch;
            activate_candidate(browser, index, query_generation, epoch, token, cx)
        }
        _ => false,
    }
}

/// Select a completion row from pointer hover.
fn hover_candidate(
    browser: &mut Browser,
    index: usize,
    generation: u64,
    epoch: u64,
    token: CandidateToken,
    cx: &mut Context<Browser>,
) {
    if !candidate_is_current(&browser.tag_completion, index, generation, epoch, &token) {
        return;
    }
    browser.tag_completion.selected = Some(index);
    cx.notify();
}

fn activate_candidate(
    browser: &mut Browser,
    index: usize,
    generation: u64,
    epoch: u64,
    token: CandidateToken,
    cx: &mut Context<Browser>,
) -> bool {
    if !candidate_is_current(&browser.tag_completion, index, generation, epoch, &token) {
        return false;
    }
    let Some(candidate) = browser.tag_completion.candidate(index) else {
        return false;
    };
    match candidate {
        MergedCandidate::Default(label) => activate_label_candidate(browser, label, cx),
        MergedCandidate::Profile { label } => activate_label_candidate(browser, &label, cx),
        MergedCandidate::Source {
            tag_id, revision, ..
        } => activate_source_candidate(browser, tag_id, revision, cx),
    }
}

fn candidate_is_current(
    state: &TagCompletionState,
    index: usize,
    generation: u64,
    epoch: u64,
    token: &CandidateToken,
) -> bool {
    generation == state.draft_generation
        && epoch == state.page_epoch
        && state
            .candidate(index)
            .is_some_and(|candidate| candidate.token().eq(token))
}

fn activate_source_candidate(
    browser: &mut Browser,
    tag_id: i64,
    page_revision: u64,
    cx: &mut Context<Browser>,
) -> bool {
    if tag_id <= 0 {
        browser.tag_completion.message = Some(String::from(
            "Source tag suggestions returned an invalid tag identity",
        ));
        return true;
    }
    let Some(query) = browser.tag_completion.desired.clone() else {
        return false;
    };
    if !tags::tag_context_is_current(browser, &query.context)
        || query.context.revision != page_revision
    {
        browser.tag_completion.message = Some(String::from(
            "Tag suggestions are stale; edit the tag to retry",
        ));
        return true;
    }
    let paths = match browser.capture_tag_target_paths(1) {
        Ok(paths) if !paths.is_empty() => paths,
        Ok(_) => {
            browser.tag_completion.message =
                Some(String::from("Select a sample before choosing a source tag"));
            return true;
        }
        Err(error) => {
            browser.tag_completion.message = Some(error);
            return true;
        }
    };
    let Some(context) = tags::capture_tag_context(browser, paths) else {
        browser.tag_completion.message = Some(String::from(
            "Source tag suggestions require a current audited catalog projection",
        ));
        return true;
    };
    if context.source_id != query.context.source_id
        || context.lifecycle_generation != query.context.lifecycle_generation
        || context.root != query.context.root
        || context.root_identity != query.context.root_identity
        || context.directory_generation != query.context.directory_generation
        || context.revision != query.context.revision
    {
        browser.tag_completion.message = Some(String::from(
            "Tag suggestions are stale; edit the tag to retry",
        ));
        return true;
    }
    browser.tag_completion.clear_query(true);
    clear_completion_ghost(browser, cx);
    let result = browser.submit_add_tag_id(tag_id, Some(context.paths), cx);
    match result {
        TagAdmissionResult::Accepted | TagAdmissionResult::NoChange => {
            browser.tag_completion.suppress_next_input = true;
            browser
                .tag_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        TagAdmissionResult::Rejected(message) => {
            browser.tag_completion.message = Some(message);
        }
    }
    true
}

fn activate_label_candidate(browser: &mut Browser, label: &str, cx: &mut Context<Browser>) -> bool {
    browser.tag_completion.clear_query(true);
    clear_completion_ghost(browser, cx);
    browser.tag_completion.suppress_next_input = true;
    browser
        .tag_input
        .update(cx, |input, cx| input.set_content(label.to_owned(), cx));
    browser.submit_tag_editor(cx);
    true
}

/// Render bounded completion rows and one composite pager.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    if tags::category_picker_active(browser) {
        return div().flex_none().w_full();
    }
    let Some(query) = browser.tag_completion.desired.as_ref() else {
        return div().flex_none().w_full();
    };
    let generation = query.draft_generation;
    let epoch = browser.tag_completion.page_epoch;
    let selected = browser.tag_completion.selected;
    let rows = browser
        .tag_completion
        .merged_page
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = selected == Some(index);
            let token = candidate.token();
            let hover_token = token.clone();
            let label = candidate.display_label();
            let hover_fill = color(theme.colors.selection_hover_fill);
            let pressed_fill = color(theme.interaction.pressed_fill);
            let text_color = if selected {
                color(theme.colors.accent)
            } else {
                color(theme.colors.text_primary)
            };
            div()
                .id(("tag-completion-row", index))
                .flex()
                .flex_none()
                .w_full()
                .h(px(theme.spacing.row_height))
                .items_center()
                .cursor_pointer()
                .bg(if selected {
                    color(theme.colors.selection_fill)
                } else {
                    transparent()
                })
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed_fill))
                .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
                    if *hovered {
                        hover_candidate(view, index, generation, epoch, hover_token.clone(), cx);
                    }
                }))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    let _ = activate_candidate(view, index, generation, epoch, token.clone(), cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .pl(px(theme.spacing.row_padding_x))
                        .text_size(px(theme.typography.small_size))
                        .text_color(text_color)
                        .child(label),
                )
        })
        .collect::<Vec<_>>();
    let row_count = rows.len();
    let list = (row_count > 0).then(|| {
        div()
            .id("tag-completion-list")
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .h(px(
                theme.spacing.row_height * row_count.min(MERGED_PAGE_SIZE) as f32
            ))
            .overflow_y_scroll()
            .track_scroll(&browser.tag_completion.scroll_handle)
            .children(rows)
    });

    let loading = has_pending(browser);
    let status = if loading {
        Some(String::from("Loading tag suggestions…"))
    } else if row_count == 0 {
        browser
            .tag_completion
            .message
            .clone()
            .or_else(|| Some(String::from("No matching tags")))
    } else {
        browser.tag_completion.message.clone()
    };

    let mut root = div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .px(px(theme.spacing.panel_padding));
    if let Some(list) = list {
        root = root.child(list);
    }
    if !browser.tag_completion.page_history.is_empty() || browser.tag_completion.page_has_next {
        let first = (!browser.tag_completion.page_history.is_empty()).then(|| {
            div()
                .id("tag-completion-first")
                .flex_1()
                .h(px(theme.spacing.row_height))
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .hover(|style| style.bg(color(theme.colors.selection_hover_fill)))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    request_first(view, epoch, cx);
                }))
                .child("First")
        });
        let previous = (!browser.tag_completion.page_history.is_empty()).then(|| {
            div()
                .id("tag-completion-previous")
                .flex_1()
                .h(px(theme.spacing.row_height))
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .hover(|style| style.bg(color(theme.colors.selection_hover_fill)))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    request_previous(view, epoch, cx);
                }))
                .child("Previous")
        });
        let next = browser.tag_completion.page_has_next.then(|| {
            div()
                .id("tag-completion-next")
                .flex_1()
                .h(px(theme.spacing.row_height))
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .hover(|style| style.bg(color(theme.colors.selection_hover_fill)))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    request_next(view, epoch, cx);
                }))
                .child("Next")
        });
        root = root.child(
            div()
                .flex()
                .flex_none()
                .w_full()
                .gap(px(theme.spacing.control_gap))
                .children(first)
                .children(previous)
                .children(next),
        );
    }
    if let Some(status) = status {
        root = root.child(
            div()
                .flex_none()
                .w_full()
                .h(px(theme.spacing.row_height))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(status),
        );
    }
    root
}

impl MergedCandidate {
    fn token(&self) -> CandidateToken {
        match self {
            Self::Default(label) => CandidateToken::Default(label),
            Self::Profile { label } => CandidateToken::Profile(label.clone()),
            Self::Source { tag_id, .. } => CandidateToken::Source(*tag_id),
        }
    }

    fn display_label(&self) -> String {
        match self {
            Self::Default(label) => String::from(*label),
            Self::Profile { label } => label.clone(),
            Self::Source { label, .. } => label.clone(),
        }
    }

    fn ghost_suffix(&self, prefix: &str) -> Option<String> {
        let label = match self {
            Self::Default(label) => *label,
            Self::Profile { label } => label.as_str(),
            Self::Source {
                exact_label: Some(label),
                ..
            } => label.as_str(),
            Self::Source {
                exact_label: None, ..
            } => return None,
        };
        label
            .strip_prefix(prefix)
            .filter(|suffix| !suffix.is_empty())
            .map(String::from)
    }
}

fn normalized_completion_prefix(input: &str) -> Option<String> {
    if input.contains([',', ';', '\n']) {
        return None;
    }
    let labels = parse_tag_input(input).ok()?;
    let prefix = (labels.len() == 1)
        .then(|| labels.into_iter().next())
        .flatten()?;
    (prefix.len() <= SOURCE_TAG_PAGE_MAX_PREFIX_BYTES).then_some(prefix)
}

fn next_selection(
    selected: Option<usize>,
    count: usize,
    direction: SelectionDirection,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (selected, direction) {
        (None, SelectionDirection::Previous) => count - 1,
        (None, SelectionDirection::Next) => 0,
        (Some(index), SelectionDirection::Previous) => index.checked_sub(1).unwrap_or(count - 1),
        (Some(index), SelectionDirection::Next) => (index + 1) % count,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CURSOR_HISTORY, MergedCandidate, ProfilePageWindow, SelectionDirection,
        SourcePageWindow, StreamCheckpoint, TagCompletionState, apply_stream_checkpoint,
        candidate_is_current, install_source_page, merge_ready_page, next_selection,
        normalized_completion_prefix, push_page_history,
    };
    use std::path::Path;

    use tempfile::{TempDir, tempdir};
    use wavecrate_library::sample_sources::{
        ProfileTagCategory, ProfileTagVocabularyEntry, SourceDatabase, SourceTagPage,
        SourceTagPageEntry,
    };

    #[test]
    fn prefix_is_normalized_only_for_one_undelimited_draft() {
        assert_eq!(
            normalized_completion_prefix("  Deep--Kick!! 01  "),
            Some(String::from("deep-kick-01"))
        );
        assert_eq!(normalized_completion_prefix("kick, warm"), None);
        assert_eq!(normalized_completion_prefix("kick;warm"), None);
        assert_eq!(normalized_completion_prefix("kick\nwarm"), None);
        assert_eq!(normalized_completion_prefix("---"), None);
    }

    #[test]
    fn selection_requires_an_explicit_arrow_and_wraps_within_the_window() {
        assert_eq!(next_selection(None, 3, SelectionDirection::Next), Some(0));
        assert_eq!(
            next_selection(None, 3, SelectionDirection::Previous),
            Some(2)
        );
        assert_eq!(
            next_selection(Some(0), 3, SelectionDirection::Previous),
            Some(2)
        );
        assert_eq!(
            next_selection(Some(2), 3, SelectionDirection::Next),
            Some(0)
        );
        assert_eq!(next_selection(None, 0, SelectionDirection::Next), None);
    }

    #[test]
    fn composite_cursor_history_stays_bounded() {
        let mut history = Vec::new();
        for _ in 0..(MAX_CURSOR_HISTORY + 3) {
            push_page_history(&mut history, StreamCheckpoint::initial());
        }
        assert_eq!(history.len(), MAX_CURSOR_HISTORY);
    }

    #[test]
    fn ghost_suffix_is_only_offered_for_exact_canonical_candidates() {
        assert_eq!(
            MergedCandidate::Profile {
                label: String::from("deep-kick"),
            }
            .ghost_suffix("deep"),
            Some(String::from("-kick"))
        );
        assert_eq!(
            MergedCandidate::Source {
                tag_id: 17,
                label: String::from("legacy…"),
                exact_label: None,
                revision: 4,
            }
            .ghost_suffix("legacy"),
            None
        );
    }

    #[test]
    fn merge_keeps_source_identity_for_exact_duplicates_and_stays_bounded() {
        let mut state = TagCompletionState::default();
        state.source_ready = true;
        state.profile_ready = true;
        state.source_page = Some(SourcePageWindow {
            revision: 7,
            entries: (0..64)
                .map(|index| SourceTagPageEntry {
                    id: 1000 + index,
                    display_label: format!("tag-{:03}", index * 2),
                    normalized_text: format!("tag-{:03}", index * 2),
                    assignment_count: None,
                    is_preview: false,
                })
                .collect(),
            next_cursor: None,
            page_start: None,
            offset: 0,
        });
        state.profile_page = Some(super::ProfilePageWindow {
            revision: 3,
            entries: (0..64)
                .map(|index| ProfileTagVocabularyEntry {
                    label: format!("tag-{:03}", index),
                    category: ProfileTagCategory::SoundType,
                })
                .collect(),
            next_cursor: None,
            page_start: None,
            offset: 0,
        });

        let merge = merge_ready_page(&mut state, "tag");

        assert_eq!(merge.source_after, None);
        assert_eq!(merge.profile_after, None);
        assert_eq!(state.merged_page.len(), 6);
        assert_eq!(
            state.merged_page[0].token(),
            super::CandidateToken::Source(1000)
        );
        assert_eq!(state.merged_page[0].display_label(), "tag-000");
        assert_eq!(
            state
                .merged_page
                .iter()
                .map(MergedCandidate::display_label)
                .collect::<Vec<_>>(),
            vec![
                "tag-000", "tag-001", "tag-002", "tag-003", "tag-004", "tag-005"
            ]
        );
        assert!(state.page_has_next);
        assert!(state.merged_page.len() <= super::MERGED_PAGE_SIZE);
    }

    #[test]
    fn merge_advances_retained_windows_without_repeating_the_previous_page() {
        let mut state = TagCompletionState::default();
        state.source_ready = true;
        state.profile_ready = true;
        state.source_page = Some(SourcePageWindow {
            revision: 7,
            entries: (0..64)
                .map(|index| SourceTagPageEntry {
                    id: 1000 + index,
                    display_label: format!("tag-{:03}", index * 2),
                    normalized_text: format!("tag-{:03}", index * 2),
                    assignment_count: None,
                    is_preview: false,
                })
                .collect(),
            next_cursor: None,
            page_start: None,
            offset: 0,
        });
        state.profile_page = Some(super::ProfilePageWindow {
            revision: 3,
            entries: (0..64)
                .map(|index| ProfileTagVocabularyEntry {
                    label: format!("tag-{:03}", index * 2 + 1),
                    category: ProfileTagCategory::SoundType,
                })
                .collect(),
            next_cursor: None,
            page_start: None,
            offset: 0,
        });

        merge_ready_page(&mut state, "tag");
        let first_page = state
            .merged_page
            .iter()
            .map(MergedCandidate::display_label)
            .collect::<Vec<_>>();
        state.merged_page.clear();
        merge_ready_page(&mut state, "tag");
        let second_page = state
            .merged_page
            .iter()
            .map(MergedCandidate::display_label)
            .collect::<Vec<_>>();

        assert_eq!(first_page[0], "tag-000");
        assert_eq!(first_page.last().map(String::as_str), Some("tag-005"));
        assert_eq!(second_page.first().map(String::as_str), Some("tag-006"));
        assert_eq!(second_page.last().map(String::as_str), Some("tag-011"));
        assert!(first_page.iter().all(|label| !second_page.contains(label)));
    }

    #[test]
    fn candidate_callbacks_require_generation_epoch_and_identity_match() {
        let mut state = TagCompletionState::default();
        state.draft_generation = 4;
        state.page_epoch = 9;
        state.merged_page = vec![MergedCandidate::Profile {
            label: String::from("tag-kick"),
        }];
        let token = state.merged_page[0].token();

        assert!(candidate_is_current(&state, 0, 4, 9, &token));
        assert!(!candidate_is_current(&state, 0, 3, 9, &token));
        assert!(!candidate_is_current(&state, 0, 4, 8, &token));
        assert!(!candidate_is_current(
            &state,
            0,
            4,
            9,
            &super::CandidateToken::Profile(String::from("tag-snare")),
        ));
    }

    #[test]
    fn valid_legacy_preview_survives_a_prefix_longer_than_its_preview() {
        let mut state = TagCompletionState::default();
        state.source_ready = true;
        state.profile_ready = true;
        state.source_page = Some(SourcePageWindow {
            revision: 7,
            entries: vec![SourceTagPageEntry {
                id: 41,
                display_label: String::from("legacy-tag…"),
                normalized_text: String::from("legacy"),
                assignment_count: None,
                is_preview: true,
            }],
            next_cursor: None,
            page_start: None,
            offset: 0,
        });

        merge_ready_page(&mut state, "legacy-tag");

        assert_eq!(state.merged_page.len(), 1);
        assert_eq!(
            state.merged_page[0].token(),
            super::CandidateToken::Source(41)
        );
        assert_eq!(state.merged_page[0].display_label(), "legacy-tag…");
    }

    #[test]
    fn merge_refills_at_source_page_boundary_with_profile_carry_and_source_identity() {
        let (_directory, _database, first_page, second_page) = source_page_fixture(128);
        let first_cursor = first_page
            .next_cursor
            .expect("the first bounded source page has a continuation");
        assert_eq!(second_page.entries.len(), 64);

        let mut state = TagCompletionState::default();
        state.source_ready = true;
        state.profile_ready = true;
        install_source_page(&mut state, first_page, None, 0);
        state.profile_page = Some(ProfilePageWindow {
            revision: 3,
            entries: vec![
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063"),
                    category: ProfileTagCategory::SoundType,
                },
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063a"),
                    category: ProfileTagCategory::SoundType,
                },
            ],
            next_cursor: None,
            page_start: None,
            offset: 0,
        });

        let mut boundary_page = None;
        for _ in 0..32 {
            state.merged_page.clear();
            let merge = merge_ready_page(&mut state, "tag");
            if merge.source_after.is_some() {
                boundary_page = Some(merge);
                break;
            }
        }
        let boundary = boundary_page.expect("the merge must request the second source page");
        assert_eq!(boundary.source_after, Some(first_cursor));
        assert_eq!(state.merged_page.len(), 4);
        assert_eq!(state.source_page.as_ref().map(|page| page.offset), Some(64));
        assert_eq!(state.profile_page.as_ref().map(|page| page.offset), Some(0));
        assert!(matches!(
            state.merged_page.last(),
            Some(MergedCandidate::Source { tag_id, .. }) if *tag_id == state.source_page.as_ref().unwrap().entries[63].id
        ));

        install_source_page(&mut state, second_page, Some(first_cursor), 0);
        merge_ready_page(&mut state, "tag");

        assert_eq!(
            state
                .merged_page
                .iter()
                .map(MergedCandidate::display_label)
                .collect::<Vec<_>>(),
            vec![
                "tag-060", "tag-061", "tag-062", "tag-063", "tag-063a", "tag-064"
            ]
        );
        assert!(state.merged_page.iter().any(|candidate| matches!(
            candidate,
            MergedCandidate::Source { tag_id, .. }
                if *tag_id == state.source_page.as_ref().unwrap().entries[0].id
        )));
        assert!(!state.merged_page.iter().any(|candidate| matches!(
            candidate,
            MergedCandidate::Profile { label } if label == "tag-063"
        )));
    }

    #[test]
    fn restoring_composite_checkpoint_replays_the_same_page_after_source_refill() {
        let (_directory, _database, first_page, second_page) = source_page_fixture(128);
        let first_cursor = first_page
            .next_cursor
            .expect("the first bounded source page has a continuation");

        let mut state = TagCompletionState::default();
        state.source_ready = true;
        state.profile_ready = true;
        install_source_page(&mut state, first_page.clone(), None, 0);
        state.profile_page = Some(profile_carry_page());

        consume_until_source_refill(&mut state);
        install_source_page(&mut state, second_page.clone(), Some(first_cursor), 0);
        merge_ready_page(&mut state, "tag");
        let checkpoint = state.snapshot();
        assert_eq!(checkpoint.source_after_cursor, Some(first_cursor));
        assert_eq!(checkpoint.source_offset, 1);
        assert_eq!(checkpoint.profile_offset, 2);

        state.merged_page.clear();
        merge_ready_page(&mut state, "tag");
        let expected_page = state
            .merged_page
            .iter()
            .map(MergedCandidate::display_label)
            .collect::<Vec<_>>();
        assert_eq!(
            expected_page,
            vec![
                "tag-065", "tag-066", "tag-067", "tag-068", "tag-069", "tag-070"
            ]
        );

        state.merged_page.clear();
        merge_ready_page(&mut state, "tag");
        assert_ne!(
            state
                .merged_page
                .iter()
                .map(MergedCandidate::display_label)
                .collect::<Vec<_>>(),
            expected_page
        );

        apply_stream_checkpoint(&mut state, &checkpoint);
        install_source_page(
            &mut state,
            second_page,
            checkpoint.source_after_cursor,
            checkpoint.source_offset,
        );
        state.profile_page = Some(ProfilePageWindow {
            revision: 3,
            entries: vec![
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063"),
                    category: ProfileTagCategory::SoundType,
                },
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063a"),
                    category: ProfileTagCategory::SoundType,
                },
            ],
            next_cursor: None,
            page_start: None,
            offset: checkpoint.profile_offset,
        });
        state.source_ready = true;
        state.profile_ready = true;
        merge_ready_page(&mut state, "tag");

        assert_eq!(
            state
                .merged_page
                .iter()
                .map(MergedCandidate::display_label)
                .collect::<Vec<_>>(),
            expected_page
        );
    }

    fn consume_until_source_refill(state: &mut TagCompletionState) {
        for _ in 0..32 {
            state.merged_page.clear();
            if merge_ready_page(state, "tag").source_after.is_some() {
                return;
            }
        }
        panic!("the merge did not reach the bounded source page boundary");
    }

    fn profile_carry_page() -> ProfilePageWindow {
        ProfilePageWindow {
            revision: 3,
            entries: vec![
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063"),
                    category: ProfileTagCategory::SoundType,
                },
                ProfileTagVocabularyEntry {
                    label: String::from("tag-063a"),
                    category: ProfileTagCategory::SoundType,
                },
            ],
            next_cursor: None,
            page_start: None,
            offset: 0,
        }
    }

    fn source_page_fixture(
        tag_count: usize,
    ) -> (TempDir, SourceDatabase, SourceTagPage, SourceTagPage) {
        let directory = tempdir().expect("temporary source directory");
        let database = SourceDatabase::open_for_user_metadata_write(directory.path())
            .expect("source database");
        let mut batch = database.write_batch().expect("source write batch");
        batch
            .upsert_file(Path::new("one.wav"), 10, 5)
            .expect("fixture wav row");
        for index in 0..tag_count {
            let label = format!("tag-{index:03}");
            batch
                .assign_tag_to_path(Path::new("one.wav"), &label)
                .expect("fixture tag row");
        }
        batch.commit().expect("fixture commit");
        let revision = database.get_revision().expect("source revision");
        let first_page = database
            .tag_vocabulary_page_at_revision(revision, "tag", None, 64)
            .expect("first source tag page");
        let cursor = first_page
            .next_cursor
            .expect("fixture must have two source pages");
        let second_page = database
            .tag_vocabulary_page_at_revision(revision, "tag", Some(&cursor), 64)
            .expect("second source tag page");
        (directory, database, first_page, second_page)
    }
}
