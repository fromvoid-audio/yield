#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded selected-tag pages and exact-ID removal for the browser sidebar.

use gpui::{ClickEvent, Context, ScrollHandle, div, prelude::*, px};
use yield_design::Theme;
use yield_library::sample_sources::{SourceTagPageCursor, SourceTagPageEntry};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogSubmitError, CatalogTagPageQuery,
    SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
};

use crate::theme::color;

use super::Browser;
use super::catalog;
use super::curation::TagAdmissionResult;
use super::tags::{self, TagSubmissionContext};

const MAX_CURSOR_HISTORY: usize = 32;
const MAX_PAGE_ENTRIES: usize = SOURCE_TAG_PAGE_MAX_PAGE_SIZE;
const VISIBLE_CHIP_ROWS: usize = 6;

/// Browser-owned state for one selected-tag page and its exact removal lane.
pub(super) struct TagChipsState {
    page: Option<SelectedTagPage>,
    pending: Option<PendingTagPage>,
    dirty: bool,
    generation: u64,
    requested_cursor: Option<SourceTagPageCursor>,
    cursor_history: Vec<Option<SourceTagPageCursor>>,
    message: Option<String>,
    pub(super) scroll_handle: ScrollHandle,
}

impl Default for TagChipsState {
    fn default() -> Self {
        Self {
            page: None,
            pending: None,
            dirty: true,
            generation: 0,
            requested_cursor: None,
            cursor_history: Vec::new(),
            message: None,
            scroll_handle: ScrollHandle::new(),
        }
    }
}

struct SelectedTagPage {
    context: TagSubmissionContext,
    selected_count: usize,
    entries: Vec<SourceTagPageEntry>,
    next_cursor: Option<SourceTagPageCursor>,
    page_start: Option<SourceTagPageCursor>,
}

struct PendingTagPage {
    request_id: u64,
    generation: u64,
    context: TagSubmissionContext,
    selected_count: usize,
    after_cursor: Option<SourceTagPageCursor>,
}

impl TagChipsState {
    fn mark_dirty(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.page = None;
        self.dirty = true;
        self.requested_cursor = None;
        self.cursor_history.clear();
        self.message = None;
    }

    fn cancel_for_source_change(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.page = None;
        self.pending = None;
        self.dirty = true;
        self.requested_cursor = None;
        self.cursor_history.clear();
        self.message = None;
    }

    fn invalidate_after_curation(&mut self) {
        self.cancel_for_source_change();
    }

    fn navigate(&mut self, after_cursor: Option<SourceTagPageCursor>, remember_current: bool) {
        if remember_current
            && let Some(current_start) = self.page.as_ref().map(|page| page.page_start)
        {
            push_cursor_history(&mut self.cursor_history, current_start);
        }
        self.generation = self.generation.wrapping_add(1);
        self.page = None;
        self.pending = None;
        self.dirty = true;
        self.requested_cursor = after_cursor;
        self.message = None;
    }

    fn settle_failure(&mut self, message: String) {
        self.page = None;
        self.dirty = false;
        self.message = Some(message);
    }
}

/// Mark the selected-tag page stale while retaining an in-flight same-lifecycle request.
pub(super) fn invalidate(browser: &mut Browser) {
    browser.tag_chips.mark_dirty();
    super::tag_library::invalidate(browser);
}

/// Drop selected-tag page state when the active source lifecycle is replaced.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.tag_chips.cancel_for_source_change();
    super::tag_library::cancel_for_source_change(browser);
}

/// Drop selected-tag page state after a durable curation outcome changes the revision.
pub(super) fn invalidate_after_curation(browser: &mut Browser) {
    browser.tag_chips.invalidate_after_curation();
    super::tag_library::invalidate_after_curation(browser);
}

/// Return whether the source poll lane has chip work to service.
pub(super) fn has_pending(browser: &Browser) -> bool {
    let state = &browser.tag_chips;
    if state.pending.is_some() {
        return true;
    }
    if !state.dirty || browser.curation_operation_pending() {
        return false;
    }
    tags::capture_tag_context(browser, Vec::new()).is_some()
}

/// Poll one selected-tag page request and admit at most one replacement request per tick.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser.tag_chips.pending.is_some()
        || !browser.tag_chips.dirty
        || browser.curation_operation_pending()
    {
        return false;
    }

    let paths = match browser.capture_tag_target_paths(1) {
        Ok(paths) => paths,
        Err(error) => {
            browser.tag_chips.page = None;
            browser.tag_chips.dirty = false;
            browser.tag_chips.message = Some(error);
            return true;
        }
    };
    if paths.is_empty() {
        browser.tag_chips.page = None;
        browser.tag_chips.dirty = false;
        browser.tag_chips.message = None;
        return true;
    }
    let Some(context) = tags::capture_tag_context(browser, paths.clone()) else {
        browser.tag_chips.page = None;
        browser.tag_chips.dirty = false;
        browser.tag_chips.message = Some(String::from(
            "Selected tags unavailable until the catalog is ready",
        ));
        return true;
    };
    let selected_count = paths.len();
    let after_cursor = browser.tag_chips.requested_cursor;
    let request_id = browser.next_tag_request_id();
    let command = CatalogCommand::RequestTagPage {
        source_id: context.source_id.clone(),
        lifecycle_generation: context.lifecycle_generation,
        expected_revision: context.revision,
        request_id,
        query: CatalogTagPageQuery::SelectedPaths {
            relative_paths: paths,
        },
        after_cursor,
        page_limit: SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
    };
    match catalog::submit_catalog_command(command, cx) {
        Ok(()) => {
            browser.tag_chips.pending = Some(PendingTagPage {
                request_id,
                generation: browser.tag_chips.generation,
                context,
                selected_count,
                after_cursor,
            });
            browser.tag_chips.dirty = false;
            browser.tag_chips.message = None;
            true
        }
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
            browser.tag_chips.message = Some(String::from("Selected tags waiting for catalog…"));
            true
        }
        Err(CatalogSubmitError::Unavailable(error)) => {
            browser.tag_chips.dirty = false;
            browser.tag_chips.page = None;
            browser.tag_chips.message = Some(format!("Could not load selected tags: {error}"));
            true
        }
        Err(CatalogSubmitError::Closed) => {
            browser.tag_chips.cancel_for_source_change();
            browser.tag_chips.dirty = false;
            browser.tag_chips.message = Some(String::from("Source catalog closed"));
            true
        }
        Err(CatalogSubmitError::InvalidTagPageRequest(error)) => {
            browser.tag_chips.dirty = false;
            browser.tag_chips.page = None;
            browser.tag_chips.message = Some(format!("Could not load selected tags: {error}"));
            true
        }
        Err(
            CatalogSubmitError::InvalidWavMetadataRequest(_)
            | CatalogSubmitError::InvalidCurationRequest(_)
            | CatalogSubmitError::InvalidTagFilterRequest(_)
            | CatalogSubmitError::InvalidDisplayLabelsRequest(_)
            | CatalogSubmitError::InvalidCurationTagFactsRequest(_)
            | CatalogSubmitError::InvalidIndexPageRequest(_)
            | CatalogSubmitError::InvalidRatingDecayRequest(_),
        ) => {
            browser.tag_chips.dirty = false;
            browser.tag_chips.page = None;
            browser.tag_chips.message = Some(String::from(
                "Could not load selected tags: invalid catalog request",
            ));
            true
        }
    }
}

/// Apply a selected-tag page completion when its captured source and request identity match.
pub(super) fn apply_catalog_event(browser: &mut Browser, event: &CatalogEvent) -> bool {
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
    let Some(pending) = browser.tag_chips.pending.as_ref() else {
        return false;
    };
    if pending.request_id != request_id
        || pending.context.source_id != *source_id
        || pending.context.lifecycle_generation != lifecycle_generation
    {
        return false;
    }
    let pending = browser
        .tag_chips
        .pending
        .take()
        .expect("selected-tag pending request was checked");
    let current = pending.generation == browser.tag_chips.generation
        && tags::tag_context_is_current(browser, &pending.context)
        && !browser.curation_operation_pending();
    match event {
        CatalogEvent::TagPageReady {
            root_identity,
            revision,
            page,
            ..
        } if current
            && tags::source_page_reply_is_coherent(
                &pending.context,
                source_id,
                lifecycle_generation,
                root_identity,
                *revision,
                page.revision,
            )
            && page.entries.len() <= MAX_PAGE_ENTRIES
            && page
                .entries
                .iter()
                .all(|entry| entry.assignment_count.is_some()) =>
        {
            browser.tag_chips.page = Some(SelectedTagPage {
                context: pending.context,
                selected_count: pending.selected_count,
                entries: page.entries.clone(),
                next_cursor: page.next_cursor,
                page_start: pending.after_cursor,
            });
            browser.tag_chips.dirty = false;
            browser.tag_chips.requested_cursor = None;
            browser.tag_chips.message = None;
        }
        CatalogEvent::TagPageFailed {
            expected_revision,
            observed_revision,
            error,
            ..
        } if matches!(
            classify_tag_page_failure(current, *expected_revision, pending.context.revision),
            TagPageFailureDisposition::Terminal
        ) =>
        {
            let message =
                if observed_revision.is_some_and(|revision| revision != pending.context.revision) {
                    String::from("Selected tags are stale; refresh the catalog before retrying")
                } else {
                    format!("Could not load selected tags: {error}")
                };
            browser.tag_chips.settle_failure(message);
        }
        _ => {
            browser.tag_chips.page = None;
            browser.tag_chips.dirty = true;
            browser.tag_chips.requested_cursor = None;
            browser.tag_chips.cursor_history.clear();
            browser.tag_chips.message = Some(String::from(
                "Selected tags response was stale; refreshing…",
            ));
        }
    }
    true
}

/// Render selected tag chips below the add-tag editor.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    if browser.active_source.is_none() {
        return div().flex_none().w_full();
    }
    let pending = browser.tag_chips.pending.is_some();
    let mutation_pending = browser.curation_operation_pending();
    let page = browser.tag_chips.page.as_ref();
    let selected_count = page.map(|page| page.selected_count).unwrap_or(0);
    let entries = page.map(|page| page.entries.as_slice()).unwrap_or(&[]);
    let next_cursor = page.and_then(|page| page.next_cursor);
    let page_start = page.and_then(|page| page.page_start);
    let history_has_previous = !browser.tag_chips.cursor_history.is_empty();
    let dirty = browser.tag_chips.dirty;
    let message = browser.tag_chips.message.clone();
    let header = div()
        .flex()
        .flex_none()
        .w_full()
        .items_center()
        .h(px(theme.spacing.row_height))
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .child(if selected_count == 0 {
            String::from("SELECTED TAGS")
        } else {
            format!("SELECTED TAGS · {selected_count}")
        });

    let rows = entries
        .iter()
        .map(|entry| {
            let label = display_label(entry);
            let assignment = assignment_state(entry, selected_count);
            let assignment_label = assignment.label();
            let tag_id = entry.id;
            let generation = browser.tag_chips.generation;
            let remove_hover = color(theme.colors.selection_hover_fill);
            let remove_pressed = color(theme.interaction.pressed_fill);
            let remove_color = if mutation_pending {
                color(theme.colors.text_disabled)
            } else {
                color(theme.colors.text_muted)
            };
            let remove_button = div()
                .id(("selected-tag-remove", tag_id as u64))
                .flex_none()
                .w(px(theme.spacing.control_height))
                .h(px(theme.spacing.control_height))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.small_size))
                .text_color(remove_color)
                .when(!mutation_pending, |element| {
                    element
                        .cursor_pointer()
                        .hover(move |style| style.bg(remove_hover))
                        .active(move |style| style.bg(remove_pressed))
                })
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    if !mutation_pending {
                        view.remove_tag_chip(tag_id, generation, cx);
                    }
                }))
                .child("×");
            div()
                .id(("selected-tag-chip", tag_id as u64))
                .flex()
                .flex_none()
                .w_full()
                .min_w_0()
                .h(px(theme.spacing.control_height))
                .items_center()
                .rounded(px(3.0))
                .bg(color(theme.colors.selection_fill))
                .border(px(theme.spacing.border_width))
                .border_color(color(theme.colors.border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .pl(px(theme.spacing.row_padding_x))
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(theme.colors.text_primary))
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .px(px(theme.spacing.row_padding_x))
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(if assignment == AssignmentState::All {
                            theme.colors.accent
                        } else {
                            theme.colors.text_muted
                        }))
                        .child(assignment_label),
                )
                .child(remove_button)
        })
        .collect::<Vec<_>>();
    let row_height = theme.spacing.control_height + theme.spacing.control_gap;
    let chip_list = div()
        .id("selected-tag-list")
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap(px(theme.spacing.control_gap))
        .children(rows)
        .track_scroll(&browser.tag_chips.scroll_handle);
    let chip_list = if entries.len() > VISIBLE_CHIP_ROWS {
        chip_list
            .h(px(row_height * VISIBLE_CHIP_ROWS as f32))
            .overflow_y_scroll()
    } else {
        chip_list
    };
    let body = if entries.is_empty() {
        let label = if mutation_pending {
            "Saving selected tags…"
        } else if pending || dirty {
            "Loading selected tags…"
        } else if browser.tag_chips.message.is_some() {
            "Selected tags unavailable"
        } else {
            "No tags assigned"
        };
        div()
            .id("selected-tag-empty")
            .flex()
            .flex_none()
            .w_full()
            .h(px(theme.spacing.row_height))
            .items_center()
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_muted))
            .child(label)
    } else {
        chip_list
    };
    let controls = render_page_controls(
        theme.clone(),
        cx,
        page_start.is_some(),
        history_has_previous,
        next_cursor.is_some(),
    );
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .px(px(theme.spacing.panel_padding))
        .pb(px(theme.spacing.control_gap))
        .border_t(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .child(header)
        .child(body)
        .children(controls)
        .when(message.is_some(), |element| {
            element.child(
                div()
                    .flex_none()
                    .w_full()
                    .h(px(theme.spacing.row_height))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.warning))
                    .child(message.clone().unwrap_or_default()),
            )
        })
}

fn render_page_controls(
    theme: Theme,
    cx: &mut Context<Browser>,
    has_first: bool,
    has_previous: bool,
    has_next: bool,
) -> Option<gpui::Div> {
    if !has_first && !has_previous && !has_next {
        return None;
    }
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let control = |id: &'static str, label: &'static str, enabled: bool| {
        div()
            .id(id)
            .flex()
            .flex_1()
            .h(px(theme.spacing.row_height))
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .text_color(if enabled {
                color(theme.colors.text_muted)
            } else {
                color(theme.colors.text_disabled)
            })
            .when(enabled, |element| {
                element
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .active(move |style| style.bg(pressed))
            })
            .child(label)
    };
    let first = control("selected-tags-first", "First", has_first).on_click(cx.listener(
        move |view, _: &ClickEvent, _, cx| {
            if has_first {
                view.request_tag_page_first(cx);
            }
        },
    ));
    let previous = control("selected-tags-previous", "Prev", has_previous).on_click(cx.listener(
        move |view, _: &ClickEvent, _, cx| {
            if has_previous {
                view.request_tag_page_previous(cx);
            }
        },
    ));
    let next = control("selected-tags-next", "Next", has_next).on_click(cx.listener(
        move |view, _: &ClickEvent, _, cx| {
            if has_next {
                view.request_tag_page_next(cx);
            }
        },
    ));
    Some(
        div()
            .flex()
            .flex_none()
            .w_full()
            .gap(px(theme.spacing.control_gap))
            .pt(px(theme.spacing.control_gap))
            .children([first, previous, next]),
    )
}

fn display_label(entry: &SourceTagPageEntry) -> String {
    if entry.is_preview && !entry.display_label.ends_with('…') {
        format!("{}…", entry.display_label)
    } else {
        entry.display_label.clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TagPageFailureDisposition {
    Terminal,
    Refresh,
}

fn classify_tag_page_failure(
    current: bool,
    expected_revision: u64,
    captured_revision: u64,
) -> TagPageFailureDisposition {
    (current && expected_revision == captured_revision)
        .then_some(TagPageFailureDisposition::Terminal)
        .unwrap_or(TagPageFailureDisposition::Refresh)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssignmentState {
    All,
    Mixed,
}

impl AssignmentState {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Mixed => "Mixed",
        }
    }
}

fn assignment_state(entry: &SourceTagPageEntry, selected_count: usize) -> AssignmentState {
    (entry.assignment_count == Some(selected_count as u64))
        .then_some(AssignmentState::All)
        .unwrap_or(AssignmentState::Mixed)
}

fn push_cursor_history<T>(history: &mut Vec<T>, cursor: T) {
    if history.len() >= MAX_CURSOR_HISTORY {
        history.remove(0);
    }
    history.push(cursor);
}

impl Browser {
    /// Navigate to the first selected-tag page.
    pub(super) fn request_tag_page_first(&mut self, cx: &mut Context<Self>) {
        if self.tag_chips.pending.is_some() {
            return;
        }
        self.tag_chips.cursor_history.clear();
        self.tag_chips.navigate(None, false);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Navigate to the previous selected-tag page using bounded cursor history.
    pub(super) fn request_tag_page_previous(&mut self, cx: &mut Context<Self>) {
        if self.tag_chips.pending.is_some() {
            return;
        }
        let Some(after_cursor) = self.tag_chips.cursor_history.pop() else {
            return;
        };
        self.tag_chips.navigate(after_cursor, false);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Navigate to the next selected-tag page.
    pub(super) fn request_tag_page_next(&mut self, cx: &mut Context<Self>) {
        if self.tag_chips.pending.is_some() {
            return;
        }
        let Some(next_cursor) = self
            .tag_chips
            .page
            .as_ref()
            .and_then(|page| page.next_cursor)
        else {
            return;
        };
        self.tag_chips.navigate(Some(next_cursor), true);
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Remove one selected-tag assignment by the exact persisted source tag identifier.
    pub(super) fn remove_tag_chip(&mut self, tag_id: i64, generation: u64, cx: &mut Context<Self>) {
        if self.curation_operation_pending() {
            self.tag_chips.message = Some(String::from("Tag removal is still pending"));
            cx.notify();
            return;
        }
        let Some(page) = self.tag_chips.page.as_ref() else {
            return;
        };
        if generation != self.tag_chips.generation
            || !page.entries.iter().any(|entry| entry.id == tag_id)
        {
            self.tag_chips.mark_dirty();
            self.tag_chips.message = Some(String::from("Selected tags changed; refreshing…"));
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        let page_context = page.context.clone();
        let page_selected_count = page.selected_count;
        let Ok(current_paths) = self.capture_tag_target_paths(1) else {
            self.tag_chips.mark_dirty();
            self.tag_chips.message = Some(String::from("Selected tags changed; refreshing…"));
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        };
        if current_paths != page_context.paths || current_paths.len() != page_selected_count {
            self.tag_chips.mark_dirty();
            self.tag_chips.message = Some(String::from("Selected tags changed; refreshing…"));
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        if !tags::tag_context_is_current(self, &page_context) {
            self.tag_chips.mark_dirty();
            self.tag_chips.message = Some(String::from("Selected tags changed; refreshing…"));
            self.start_source_registry_poll(cx);
            cx.notify();
            return;
        }
        match self.submit_remove_tag_id(tag_id, Some(page_context.paths), cx) {
            TagAdmissionResult::Accepted => {}
            TagAdmissionResult::NoChange => {
                self.tag_chips.invalidate_after_curation();
                self.tag_chips.message = Some(String::from("No tag changes"));
            }
            TagAdmissionResult::Rejected(message) => {
                self.tag_chips.message = Some(message);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{AssignmentState, MAX_CURSOR_HISTORY, assignment_state, push_cursor_history};
    use super::{
        PendingTagPage, SelectedTagPage, TagChipsState, TagPageFailureDisposition,
        classify_tag_page_failure,
    };
    use std::path::PathBuf;
    use yield_library::sample_sources::{
        SourceId, SourceTagPageEntry, reconciliation::RootIdentity,
    };

    fn context() -> super::TagSubmissionContext {
        super::TagSubmissionContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/source"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 4,
            revision: 5,
            paths: vec![PathBuf::from("kick.wav")],
        }
    }

    #[test]
    fn assignment_state_uses_deduplicated_selection_count() {
        let entry = SourceTagPageEntry {
            id: 17,
            display_label: String::from("kick"),
            normalized_text: String::from("kick"),
            assignment_count: Some(129),
            is_preview: false,
        };
        assert_eq!(assignment_state(&entry, 129), AssignmentState::All);
        assert_eq!(assignment_state(&entry, 128), AssignmentState::Mixed);
    }

    #[test]
    fn cursor_history_is_bounded() {
        let mut history = Vec::new();
        for cursor in 0..(MAX_CURSOR_HISTORY + 8) {
            push_cursor_history(&mut history, cursor);
        }
        assert_eq!(history.len(), MAX_CURSOR_HISTORY);
        assert_eq!(history.first(), Some(&8));
        assert_eq!(history.last(), Some(&(MAX_CURSOR_HISTORY + 7)));
    }

    #[test]
    fn selection_invalidation_coalesces_behind_the_pending_reply() {
        let mut state = TagChipsState::default();
        state.page = Some(SelectedTagPage {
            context: context(),
            selected_count: 1,
            entries: Vec::new(),
            next_cursor: None,
            page_start: None,
        });
        state.pending = Some(PendingTagPage {
            request_id: 9,
            generation: state.generation,
            context: context(),
            selected_count: 1,
            after_cursor: None,
        });
        state.dirty = false;
        let generation = state.generation;
        state.mark_dirty();
        assert!(state.pending.is_some());
        assert!(state.page.is_none());
        assert!(state.dirty);
        assert_ne!(state.generation, generation);
    }

    #[test]
    fn source_invalidation_drops_pending_correlation() {
        let mut state = TagChipsState::default();
        state.pending = Some(PendingTagPage {
            request_id: 9,
            generation: state.generation,
            context: context(),
            selected_count: 1,
            after_cursor: None,
        });
        state.cancel_for_source_change();
        assert!(state.pending.is_none());
        assert!(state.dirty);
    }

    #[test]
    fn current_revision_mismatch_failure_settles_without_retry() {
        assert_eq!(
            classify_tag_page_failure(true, 5, 5),
            TagPageFailureDisposition::Terminal
        );
        assert_eq!(
            classify_tag_page_failure(false, 5, 5),
            TagPageFailureDisposition::Refresh
        );

        let mut state = TagChipsState::default();
        state.dirty = true;
        state.settle_failure(String::from("catalog revision changed"));
        assert!(!state.dirty);
        assert!(state.page.is_none());
        assert!(state.message.is_some());
    }
}
