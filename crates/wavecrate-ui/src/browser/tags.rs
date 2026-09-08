#![deny(missing_docs)]
#![deny(warnings)]

//! Browser-owned add-tag input, vocabulary lookup, and category selection.

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;

use gpui::{ClickEvent, Context, MouseButton, MouseDownEvent, Window, div, prelude::*, px};
use wavecrate_core::{
    DEFAULT_METADATA_TAGS, MAX_TAGS_PER_COMMIT, TAG_CATEGORIES, TagCategory, infer_tag_category,
    parse_tag_input,
};
use wavecrate_design::Theme;
use wavecrate_library::sample_sources::{
    LibraryError, SourceId, SourceTagPage, reconciliation::RootIdentity,
};
use wavecrate_session::{
    CatalogCommand, CatalogEvent, CatalogTagPageQuery, ProfileTagCategory,
    ProfileTagVocabularyLookup, ProfileTagVocabularySetReceipt, VocabularyRequest,
    VocabularyStatus,
};

use crate::theme::{color, transparent};

use super::Browser;
use super::catalog;
use super::curation::{CurationLifecycleKey, TagAdmissionResult};
use super::source_registry::SourceRegistryGlobal;

const SOURCE_TAG_PAGE_LIMIT: usize = 1;

/// Browser state for one bounded add-tag intent.
pub(super) struct TagEditorState {
    draft_generation: u64,
    pending: Option<PendingTagOperation>,
    message: Option<String>,
    next_source_request_id: u64,
}

impl Default for TagEditorState {
    fn default() -> Self {
        Self {
            draft_generation: 0,
            pending: None,
            message: None,
            next_source_request_id: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TagSubmissionContext {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
    pub(super) root: PathBuf,
    pub(super) root_identity: RootIdentity,
    pub(super) directory_generation: u64,
    pub(super) revision: u64,
    pub(super) paths: Vec<PathBuf>,
}

struct PendingTagOperation {
    draft_generation: u64,
    context: TagSubmissionContext,
    labels: Vec<String>,
    profile_request: Option<VocabularyRequest<ProfileTagVocabularyLookup>>,
    profile_result: Option<Result<ProfileTagVocabularyLookup, String>>,
    source_request_id: u64,
    source_result: Option<Result<SourceTagPage, String>>,
    stage: TagStage,
}

enum TagStage {
    LookingUp,
    AwaitingCategory {
        category: TagCategory,
        suggested_category: TagCategory,
        revision: u64,
        retry_count: u8,
    },
    RefreshingCategory {
        request: VocabularyRequest<ProfileTagVocabularyLookup>,
        category: ProfileTagCategory,
        retry_count: u8,
    },
    WritingCategory {
        request: VocabularyRequest<ProfileTagVocabularySetReceipt>,
        category: ProfileTagCategory,
        retry_count: u8,
    },
}

impl TagEditorState {
    fn next_source_request_id(&mut self) -> u64 {
        self.next_source_request_id = self.next_source_request_id.wrapping_add(1);
        if self.next_source_request_id == 0 {
            self.next_source_request_id = 1;
        }
        self.next_source_request_id
    }

    fn has_io_pending(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| match &pending.stage {
                TagStage::LookingUp => {
                    pending.profile_request.is_some() || pending.source_result.is_none()
                }
                TagStage::AwaitingCategory { .. } => false,
                TagStage::RefreshingCategory { .. } | TagStage::WritingCategory { .. } => true,
            })
    }

    fn draft_changed(&mut self) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
        self.message = None;
    }

    fn input_rejected(&mut self) {
        self.message = Some(String::from("Tag input is too large"));
    }
}

/// Render the compact add-tag editor below the existing curation filters.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let pending = browser.tag_editor.pending.is_some();
    let active_source = browser.active_source.is_some();
    let submit_color = if active_source && !pending {
        color(theme.colors.accent)
    } else {
        color(theme.colors.text_disabled)
    };
    let input_border = color(if active_source {
        theme.colors.border
    } else {
        theme.colors.border_emphasis
    });
    let submit_hover = if active_source && !pending {
        color(theme.colors.selection_hover_fill)
    } else {
        transparent()
    };
    let submit_pressed = if active_source && !pending {
        color(theme.interaction.pressed_fill)
    } else {
        transparent()
    };
    let message = browser.tag_editor.message.clone();
    let category = browser
        .tag_editor
        .pending
        .as_ref()
        .and_then(|pending| match pending.stage {
            TagStage::AwaitingCategory {
                category,
                suggested_category,
                ..
            } => Some((category, suggested_category)),
            _ => None,
        });

    let input = div()
        .flex_1()
        .min_w_0()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(Browser::invalidate_file_handoff_from_tag_mouse),
        )
        .child(browser.tag_input.clone());
    let submit_button = div()
        .id("tag-submit")
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(theme.spacing.control_height))
        .h(px(theme.spacing.control_height))
        .border(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border_emphasis))
        .hover(move |style| style.bg(submit_hover))
        .active(move |style| style.bg(submit_pressed))
        .cursor(if active_source && !pending {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        })
        .text_size(px(theme.typography.title_size))
        .text_color(submit_color)
        .on_click(cx.listener(Browser::submit_tag_editor_from_click))
        .child("→");
    let editor = div()
        .flex()
        .flex_none()
        .w_full()
        .items_center()
        .gap(px(theme.spacing.control_gap))
        .px(px(theme.spacing.panel_padding))
        .py(px(theme.spacing.control_gap))
        .border_t(px(theme.spacing.border_width))
        .border_color(input_border)
        .child(input)
        .child(submit_button);

    let category_picker = category.map(|(selected_category, suggested_category)| {
        let choices = TAG_CATEGORIES
            .into_iter()
            .filter(|choice| choice.is_user_extensible())
            .map(|choice| {
                let selected = choice == selected_category;
                let choice_color = if selected {
                    color(theme.colors.accent)
                } else {
                    color(theme.colors.text_primary)
                };
                let hover_fill = color(theme.colors.selection_hover_fill);
                let pressed_fill = color(theme.interaction.pressed_fill);
                div()
                    .id(choice.id())
                    .flex()
                    .flex_none()
                    .w_full()
                    .items_center()
                    .h(px(theme.spacing.row_height))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(if selected {
                        color(theme.colors.selection_fill)
                    } else {
                        transparent()
                    })
                    .hover(move |style| style.bg(hover_fill))
                    .active(move |style| style.bg(pressed_fill))
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        view.choose_tag_category(choice, cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(theme.spacing.selected_marker_width))
                            .h_full()
                            .bg(if selected {
                                color(theme.colors.accent)
                            } else {
                                transparent()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .pl(px(theme.spacing.row_padding_x))
                            .text_size(px(theme.typography.small_size))
                            .text_color(choice_color)
                            .child(choice.label()),
                    )
            });
        div()
            .flex()
            .flex_none()
            .w_full()
            .flex_col()
            .gap(px(theme.spacing.control_gap))
            .px(px(theme.spacing.panel_padding))
            .pb(px(theme.spacing.control_gap))
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .h(px(theme.spacing.row_height))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child("Choose a category"),
            )
            .children(choices)
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .h(px(theme.spacing.row_height))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child(format!("Suggested: {}", suggested_category.label())),
            )
    });

    let status = message.map(|message| {
        div()
            .flex_none()
            .w_full()
            .px(px(theme.spacing.panel_padding))
            .pb(px(theme.spacing.control_gap))
            .h(px(theme.spacing.row_height))
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
        .w_full()
        .child(editor)
        .children(category_picker)
        .children(status)
}

/// Advance the editor generation when its native input changes.
pub(super) fn on_changed(browser: &mut Browser, _input: &str, cx: &mut Context<Browser>) {
    browser.tag_editor.draft_changed();
    cx.notify();
}

/// Show a bounded rejection from the native tag input without changing its text.
pub(super) fn on_rejected(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_editor.input_rejected();
    cx.notify();
}

/// Return whether the category chooser currently owns tag-input keyboard focus.
pub(super) fn category_picker_active(browser: &Browser) -> bool {
    browser
        .tag_editor
        .pending
        .as_ref()
        .is_some_and(|pending| matches!(pending.stage, TagStage::AwaitingCategory { .. }))
}

/// Handle keyboard commands while the tag input owns focus.
pub(super) fn handle_focused_key_down(
    browser: &mut Browser,
    key: &str,
    is_held: bool,
    is_composing: bool,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
    window: &mut Window,
    cx: &mut Context<Browser>,
) -> bool {
    if key == "escape" {
        browser.cancel_tag_editor(window, cx);
        return true;
    }
    if is_held {
        return true;
    }
    if key == "enter" && !is_composing {
        if confirm_category(browser, cx) {
            return true;
        }
        browser.submit_tag_editor(cx);
        return true;
    }
    if !secondary && !shift && !alt && !function {
        let direction = match key {
            "up" | "arrowup" => Some(-1),
            "down" | "arrowdown" => Some(1),
            _ => None,
        };
        if let Some(direction) = direction {
            cycle_category(browser, direction, cx);
        }
    }
    true
}

fn confirm_category(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(category) =
        browser
            .tag_editor
            .pending
            .as_ref()
            .and_then(|pending| match pending.stage {
                TagStage::AwaitingCategory { category, .. } => Some(category),
                _ => None,
            })
    else {
        return false;
    };
    browser.choose_tag_category(category, cx);
    true
}

fn cycle_category(browser: &mut Browser, direction: i8, cx: &mut Context<Browser>) -> bool {
    let Some(pending) = browser.tag_editor.pending.as_mut() else {
        return false;
    };
    let (next, label, suggested_category) = match &mut pending.stage {
        TagStage::AwaitingCategory {
            category,
            suggested_category,
            ..
        } => {
            let Some(next) = next_extensible_category(*category, direction) else {
                return false;
            };
            *category = next;
            (next, pending.labels[0].clone(), *suggested_category)
        }
        _ => return false,
    };
    browser.tag_editor.message = Some(format!(
        "Choose a category for {label} (selected: {}; suggested: {})",
        next.label(),
        suggested_category.label()
    ));
    cx.notify();
    true
}

fn next_extensible_category(current: TagCategory, direction: i8) -> Option<TagCategory> {
    let mut categories = TAG_CATEGORIES
        .into_iter()
        .filter(|category| category.is_user_extensible());
    let count = categories.clone().count();
    let current_index = categories.position(|category| category == current)?;
    let next_index = (current_index as isize + direction as isize).rem_euclid(count as isize);
    TAG_CATEGORIES
        .into_iter()
        .filter(|category| category.is_user_extensible())
        .nth(next_index as usize)
}

/// Poll one profile vocabulary handle and one source tag page without starting another timer.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(mut pending) = browser.tag_editor.pending.take() else {
        return false;
    };
    if !tag_context_is_current(browser, &pending.context)
        && !matches!(pending.stage, TagStage::WritingCategory { .. })
    {
        browser.tag_editor.message = Some(String::from(
            "Tag lookup canceled because the active source changed",
        ));
        return true;
    }

    let mut changed = false;
    let mut terminal = false;
    match &mut pending.stage {
        TagStage::LookingUp => {
            if let Some(mut request) = pending.profile_request.take() {
                match request.try_recv() {
                    Ok(Ok(lookup)) => {
                        pending.profile_result = Some(Ok(lookup));
                        changed = true;
                    }
                    Ok(Err(error)) => {
                        pending.profile_result = Some(Err(error.to_string()));
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        pending.profile_result =
                            Some(Err(String::from("profile vocabulary request disconnected")));
                        changed = true;
                    }
                }
                if pending.profile_result.is_none() {
                    pending.profile_request = Some(request);
                }
            }
            if pending.profile_result.is_some() && pending.source_result.is_some() {
                terminal = !finish_lookup(browser, &mut pending, cx);
                changed = true;
            }
        }
        TagStage::AwaitingCategory { .. } => {}
        TagStage::RefreshingCategory { request, .. } => {
            let result = request.try_recv();
            match result {
                Ok(Ok(lookup)) => {
                    let stage = std::mem::replace(&mut pending.stage, TagStage::LookingUp);
                    let TagStage::RefreshingCategory {
                        category,
                        retry_count,
                        ..
                    } = stage
                    else {
                        unreachable!("category refresh stage was checked")
                    };
                    changed = true;
                    if let Some(entry) = lookup
                        .entries
                        .iter()
                        .find(|entry| entry.label == pending.labels[0])
                    {
                        if entry.category == category {
                            admit_pending(browser, pending, cx);
                            return true;
                        } else {
                            browser.tag_editor.message = Some(String::from(
                                "Tag category changed concurrently; assignment canceled",
                            ));
                            terminal = true;
                        }
                    } else {
                        let label = pending.labels[0].clone();
                        match submit_category_write(browser, &label, lookup.revision, category, cx)
                        {
                            Ok(request) => {
                                pending.stage = TagStage::WritingCategory {
                                    request,
                                    category,
                                    retry_count,
                                };
                            }
                            Err(error) => {
                                browser.tag_editor.message = Some(error);
                                terminal = true;
                            }
                        }
                    }
                }
                Ok(Err(error)) => {
                    browser.tag_editor.message =
                        Some(format!("Could not refresh tag category: {error}"));
                    changed = true;
                    terminal = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    browser.tag_editor.message =
                        Some(String::from("Profile vocabulary request disconnected"));
                    changed = true;
                    terminal = true;
                }
            }
        }
        TagStage::WritingCategory { request, .. } => {
            let result = request.try_recv();
            match result {
                Ok(Ok(_receipt)) => {
                    let stage = std::mem::replace(&mut pending.stage, TagStage::LookingUp);
                    let TagStage::WritingCategory { .. } = stage else {
                        unreachable!("category write stage was checked")
                    };
                    changed = true;
                    if tag_context_is_current(browser, &pending.context) {
                        admit_pending(browser, pending, cx);
                        return true;
                    } else {
                        browser.tag_editor.message = Some(String::from(
                            "Tag category saved; file assignment canceled because the source changed",
                        ));
                        terminal = true;
                    }
                }
                Ok(Err(error)) => {
                    let stage = std::mem::replace(&mut pending.stage, TagStage::LookingUp);
                    let TagStage::WritingCategory {
                        category,
                        retry_count,
                        ..
                    } = stage
                    else {
                        unreachable!("category write stage was checked")
                    };
                    changed = true;
                    if matches!(
                        error,
                        LibraryError::ProfileTagVocabularyStaleRevision { .. }
                    ) && retry_count == 0
                        && browser.tag_editor.draft_generation == pending.draft_generation
                        && tag_context_is_current(browser, &pending.context)
                    {
                        let label = pending.labels[0].clone();
                        match submit_profile_refresh(browser, label, cx) {
                            Ok(request) => {
                                pending.stage = TagStage::RefreshingCategory {
                                    request,
                                    category,
                                    retry_count: 1,
                                };
                            }
                            Err(message) => {
                                browser.tag_editor.message = Some(message);
                                terminal = true;
                            }
                        }
                    } else {
                        browser.tag_editor.message =
                            Some(format!("Could not save tag category: {error}"));
                        terminal = true;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    browser.tag_editor.message =
                        Some(String::from("Profile vocabulary request disconnected"));
                    changed = true;
                    terminal = true;
                }
            }
        }
    }

    if !terminal {
        browser.tag_editor.pending = Some(pending);
    }
    changed || terminal
}

/// Return whether a tag request remains active for the source poll lane.
pub(super) fn has_pending(browser: &Browser) -> bool {
    browser.tag_editor.has_io_pending()
}

/// Cancel a visible tag intent when the active source lifecycle changes.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    let admitted_category_write = browser
        .tag_editor
        .pending
        .as_ref()
        .is_some_and(|pending| matches!(pending.stage, TagStage::WritingCategory { .. }));
    if admitted_category_write {
        browser.tag_editor.message = Some(String::from(
            "Source changed; finishing the tag category save…",
        ));
    } else {
        browser.tag_editor.pending = None;
        browser.tag_editor.message = None;
    }
}

/// Consume a source tag-page completion for the matching captured intent.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    _cx: &mut Context<Browser>,
) -> bool {
    let (source_id, lifecycle_generation, request_id, root_identity, revision) = match event {
        CatalogEvent::TagPageReady {
            source_id,
            lifecycle_generation,
            root_identity,
            revision,
            request_id,
            page: _page,
        } => (
            source_id,
            *lifecycle_generation,
            *request_id,
            Some(root_identity),
            Some(*revision),
        ),
        CatalogEvent::TagPageFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            ..
        } => (
            source_id,
            *lifecycle_generation,
            *request_id,
            None,
            Some(*expected_revision),
        ),
        _ => return false,
    };
    let Some(pending) = browser.tag_editor.pending.as_mut() else {
        return false;
    };
    if pending.source_request_id != request_id
        || pending.context.source_id != *source_id
        || pending.context.lifecycle_generation != lifecycle_generation
    {
        return false;
    }
    pending.source_result = Some(match event {
        CatalogEvent::TagPageReady { page, revision, .. }
            if root_identity.is_some_and(|identity| {
                source_page_reply_is_coherent(
                    &pending.context,
                    source_id,
                    lifecycle_generation,
                    identity,
                    *revision,
                    page.revision,
                )
            }) =>
        {
            Ok(page.clone())
        }
        CatalogEvent::TagPageReady { .. } => Err(String::from(
            "source tag page returned an incoherent revision",
        )),
        CatalogEvent::TagPageFailed {
            error,
            expected_revision,
            ..
        } if revision == Some(pending.context.revision)
            && *expected_revision == pending.context.revision =>
        {
            Err(error.to_string())
        }
        CatalogEvent::TagPageFailed { .. } => Err(String::from(
            "source tag page failed with an incoherent revision",
        )),
        _ => unreachable!("tag page event was checked"),
    });
    true
}

impl Browser {
    /// Allocate an identifier shared by the editor and selected-tag page requests.
    pub(super) fn next_tag_request_id(&mut self) -> u64 {
        self.tag_editor.next_source_request_id()
    }

    /// Cancel the visible tag-editor intent while letting any admitted profile write drain.
    pub(super) fn cancel_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tag_editor.pending = None;
        self.tag_editor.message = None;
        super::tag_completion::cancel_for_editor(self, cx);
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Invalidate a file handoff when the add-tag input receives pointer focus.
    pub(super) fn invalidate_file_handoff_from_tag_mouse(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.invalidate_file_handoff();
    }

    /// Submit the current add-tag input from the compact arrow button.
    pub(super) fn submit_tag_editor_from_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_tag_editor(cx);
    }

    /// Submit the current add-tag input after native parsing and bounded target capture.
    pub(super) fn submit_tag_editor(&mut self, cx: &mut Context<Self>) {
        if self.tag_editor.pending.is_some() {
            self.tag_editor.message = Some(String::from("Tag operation is still pending"));
            cx.notify();
            return;
        }
        if self.tag_input.read(cx).is_composing() {
            self.tag_editor.message = Some(String::from("Finish composing the tag first"));
            cx.notify();
            return;
        }
        let input = self.tag_input.read(cx).content().to_owned();
        let labels = match parse_tag_input(&input) {
            Ok(labels) => labels,
            Err(error) => {
                self.tag_editor.message = Some(format!("Tag input rejected: {error}"));
                cx.notify();
                return;
            }
        };
        if labels.is_empty() {
            self.tag_editor.message = Some(String::from("Enter at least one tag"));
            cx.notify();
            return;
        }
        if labels.len() > MAX_TAGS_PER_COMMIT {
            self.tag_editor.message = Some(format!(
                "Tag operation contains more than {MAX_TAGS_PER_COMMIT} labels"
            ));
            cx.notify();
            return;
        }
        let target_paths = match self.capture_tag_target_paths(labels.len()) {
            Ok(paths) if !paths.is_empty() => paths,
            Ok(_) => {
                self.tag_editor.message = Some(String::from("Select a sample before adding tags"));
                cx.notify();
                return;
            }
            Err(error) => {
                self.tag_editor.message = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(context) = capture_tag_context(self, target_paths) else {
            self.tag_editor.message = Some(String::from(
                "Tag editing requires a successful audited catalog projection",
            ));
            cx.notify();
            return;
        };
        let draft_generation = self.tag_editor.draft_generation;
        if labels.len() > 1 {
            let result = self.submit_add_tags(labels, Some(context.paths), cx);
            finish_admission(self, draft_generation, result, cx);
            return;
        }

        let label = labels[0].clone();
        let Some(global) = cx
            .has_global::<SourceRegistryGlobal>()
            .then(|| cx.global::<SourceRegistryGlobal>())
        else {
            self.tag_editor.message = Some(String::from("Profile vocabulary is unavailable"));
            cx.notify();
            return;
        };
        if matches!(global.vocabulary_status(), VocabularyStatus::Initializing) {
            self.tag_editor.message =
                Some(String::from("Profile vocabulary is still initializing"));
            cx.notify();
            return;
        }
        let profile_request = match global.lookup_profile_tag_categories(vec![label.clone()]) {
            Ok(request) => request,
            Err(error) => {
                self.tag_editor.message = Some(format!("Could not look up tag: {error}"));
                cx.notify();
                return;
            }
        };
        let source_request_id = self.next_tag_request_id();
        let source_command = CatalogCommand::RequestTagPage {
            source_id: context.source_id.clone(),
            lifecycle_generation: context.lifecycle_generation,
            expected_revision: context.revision,
            request_id: source_request_id,
            query: CatalogTagPageQuery::VocabularyPrefix { prefix: label },
            after_cursor: None,
            page_limit: SOURCE_TAG_PAGE_LIMIT,
        };
        if let Err(error) = catalog::submit_catalog_command(source_command, cx) {
            self.tag_editor.message = Some(format!("Could not inspect source tags: {error}"));
            cx.notify();
            return;
        }
        self.tag_editor.message = Some(String::from("Looking up tag…"));
        self.tag_editor.pending = Some(PendingTagOperation {
            draft_generation,
            context,
            labels,
            profile_request: Some(profile_request),
            profile_result: None,
            source_request_id,
            source_result: None,
            stage: TagStage::LookingUp,
        });
        self.start_source_registry_poll(cx);
        cx.notify();
    }

    /// Select one extensible category for the pending unknown tag.
    pub(super) fn choose_tag_category(&mut self, category: TagCategory, cx: &mut Context<Self>) {
        if !category.is_user_extensible() {
            return;
        }
        let Some(mut pending) = self.tag_editor.pending.take() else {
            return;
        };
        let TagStage::AwaitingCategory {
            revision,
            retry_count,
            ..
        } = pending.stage
        else {
            self.tag_editor.pending = Some(pending);
            return;
        };
        if self.tag_editor.draft_generation != pending.draft_generation
            || !tag_context_is_current(self, &pending.context)
        {
            self.tag_editor.message = Some(String::from(
                "Tag input or source changed; submit the tag again",
            ));
            return;
        }
        let profile_category = profile_category(category);
        let label = pending.labels[0].clone();
        match submit_category_write(self, &label, revision, profile_category, cx) {
            Ok(request) => {
                pending.stage = TagStage::WritingCategory {
                    request,
                    category: profile_category,
                    retry_count,
                };
                self.tag_editor.message =
                    Some(format!("Saving {} category…", profile_category.id()));
                self.tag_editor.pending = Some(pending);
                self.start_source_registry_poll(cx);
            }
            Err(error) => self.tag_editor.message = Some(error),
        }
        cx.notify();
    }
}

fn finish_lookup(
    browser: &mut Browser,
    pending: &mut PendingTagOperation,
    cx: &mut Context<Browser>,
) -> bool {
    let Some(profile) = pending.profile_result.as_ref() else {
        return true;
    };
    let Some(source) = pending.source_result.as_ref() else {
        return true;
    };
    let profile = match profile {
        Ok(lookup) => lookup,
        Err(error) => {
            browser.tag_editor.message = Some(format!("Could not look up tag: {error}"));
            return false;
        }
    };
    let source = match source {
        Ok(page) => page,
        Err(error) => {
            browser.tag_editor.message = Some(format!("Could not inspect source tags: {error}"));
            return false;
        }
    };
    let label = &pending.labels[0];
    let profile_entry = profile.entries.iter().find(|entry| entry.label == *label);
    let source_entry = source
        .entries
        .iter()
        .find(|entry| !entry.is_preview && entry.normalized_text == *label);
    if profile_entry.is_some()
        || source_entry.is_some()
        || DEFAULT_METADATA_TAGS.contains(&label.as_str())
        || infer_tag_category(label) == Ok(TagCategory::PlaybackType)
    {
        let labels = std::mem::take(&mut pending.labels);
        let paths = std::mem::take(&mut pending.context.paths);
        let draft_generation = pending.draft_generation;
        let result = browser.submit_add_tags(labels, Some(paths), cx);
        finish_admission(browser, draft_generation, result, cx);
        return false;
    }
    let category = match infer_tag_category(label) {
        Ok(category) => category,
        Err(error) => {
            browser.tag_editor.message = Some(format!("Could not infer tag category: {error}"));
            return false;
        }
    };
    let Some(_) = profile_category_for_inference(category) else {
        browser.tag_editor.message = Some(String::from(
            "Playback tags are built in and should be submitted directly",
        ));
        return false;
    };
    pending.stage = TagStage::AwaitingCategory {
        category,
        suggested_category: category,
        revision: profile.revision,
        retry_count: 0,
    };
    browser.tag_editor.message = Some(format!(
        "Choose a category for {} (suggested: {})",
        label,
        category.label()
    ));
    true
}

fn admit_pending(
    browser: &mut Browser,
    mut pending: PendingTagOperation,
    cx: &mut Context<Browser>,
) {
    let labels = std::mem::take(&mut pending.labels);
    let paths = std::mem::take(&mut pending.context.paths);
    let result = browser.submit_add_tags(labels, Some(paths), cx);
    finish_admission(browser, pending.draft_generation, result, cx);
}

fn finish_admission(
    browser: &mut Browser,
    draft_generation: u64,
    result: TagAdmissionResult,
    cx: &mut Context<Browser>,
) {
    match result {
        TagAdmissionResult::Accepted | TagAdmissionResult::NoChange => {
            if should_clear_after_admission(draft_generation, browser.tag_editor.draft_generation) {
                browser
                    .tag_input
                    .update(cx, |input, cx| input.set_content("", cx));
                browser.tag_editor.message = None;
            } else {
                browser.tag_editor.message = Some(String::from(
                    "Tags queued; edited input was retained for the next operation",
                ));
            }
        }
        TagAdmissionResult::Rejected(message) => browser.tag_editor.message = Some(message),
    }
    cx.notify();
}

pub(super) fn capture_tag_context(
    browser: &Browser,
    paths: Vec<PathBuf>,
) -> Option<TagSubmissionContext> {
    let activation = browser.catalog_activation.as_ref()?;
    if !activation.audit_finished || activation.audit_error.is_some() {
        return None;
    }
    let metadata = browser.catalog_metadata.context()?;
    let key = CurationLifecycleKey {
        source_id: activation.source_id.clone(),
        lifecycle_generation: activation.lifecycle_generation,
    };
    (metadata.source_id == key.source_id
        && metadata.lifecycle_generation == key.lifecycle_generation
        && metadata.root == activation.root
        && browser.curation_state.authority_matches(
            &key,
            &metadata.root_identity,
            metadata.directory_generation,
            metadata.revision,
        ))
    .then_some(TagSubmissionContext {
        source_id: key.source_id,
        lifecycle_generation: key.lifecycle_generation,
        root: metadata.root,
        root_identity: metadata.root_identity,
        directory_generation: metadata.directory_generation,
        revision: metadata.revision,
        paths,
    })
}

pub(super) fn tag_context_is_current(browser: &Browser, context: &TagSubmissionContext) -> bool {
    let Some(activation) = browser.catalog_activation.as_ref() else {
        return false;
    };
    let Some(metadata) = browser.catalog_metadata.context() else {
        return false;
    };
    let key = CurationLifecycleKey {
        source_id: context.source_id.clone(),
        lifecycle_generation: context.lifecycle_generation,
    };
    activation.source_id == context.source_id
        && activation.lifecycle_generation == context.lifecycle_generation
        && activation.root == context.root
        && metadata.source_id == context.source_id
        && metadata.lifecycle_generation == context.lifecycle_generation
        && metadata.root == context.root
        && metadata.root_identity == context.root_identity
        && metadata.directory_generation == context.directory_generation
        && metadata.revision == context.revision
        && browser.curation_state.authority_matches(
            &key,
            &context.root_identity,
            context.directory_generation,
            context.revision,
        )
}

pub(super) fn source_page_reply_is_coherent(
    context: &TagSubmissionContext,
    source_id: &SourceId,
    lifecycle_generation: u64,
    root_identity: &RootIdentity,
    event_revision: u64,
    page_revision: u64,
) -> bool {
    context.source_id == *source_id
        && context.lifecycle_generation == lifecycle_generation
        && context.root_identity == *root_identity
        && context.revision == event_revision
        && event_revision == page_revision
}

fn profile_category(category: TagCategory) -> ProfileTagCategory {
    ProfileTagCategory::from_id(category.id()).expect("core and profile categories share ids")
}

fn profile_category_for_inference(category: TagCategory) -> Option<ProfileTagCategory> {
    category
        .is_user_extensible()
        .then(|| profile_category(category))
}

fn submit_category_write(
    browser: &Browser,
    label: &str,
    revision: u64,
    category: ProfileTagCategory,
    cx: &Context<Browser>,
) -> Result<VocabularyRequest<ProfileTagVocabularySetReceipt>, String> {
    let _ = browser;
    let Some(global) = cx
        .has_global::<SourceRegistryGlobal>()
        .then(|| cx.global::<SourceRegistryGlobal>())
    else {
        return Err(String::from("Profile vocabulary is unavailable"));
    };
    global
        .set_profile_tag_category(revision, label.to_owned(), category)
        .map_err(|error| format!("Could not save tag category: {error}"))
}

fn submit_profile_refresh(
    browser: &Browser,
    label: String,
    cx: &Context<Browser>,
) -> Result<VocabularyRequest<ProfileTagVocabularyLookup>, String> {
    let _ = browser;
    let Some(global) = cx
        .has_global::<SourceRegistryGlobal>()
        .then(|| cx.global::<SourceRegistryGlobal>())
    else {
        return Err(String::from("Profile vocabulary is unavailable"));
    };
    let request = global
        .lookup_profile_tag_categories(vec![label])
        .map_err(|error| format!("Could not refresh tag category: {error}"))?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::{
        RootIdentity, SourceId, TagCategory, TagEditorState, TagSubmissionContext,
        next_extensible_category, profile_category_for_inference, should_clear_after_admission,
        source_page_reply_is_coherent,
    };
    use std::path::PathBuf;
    use wavecrate_core::infer_tag_category;

    #[test]
    fn editing_advances_generation_and_cannot_clear_new_input() {
        let mut state = TagEditorState::default();
        let submitted = state.draft_generation;
        state.draft_changed();
        assert_ne!(state.draft_generation, submitted);
        assert!(!should_clear_after_admission(
            submitted,
            state.draft_generation
        ));
    }

    #[test]
    fn playback_aliases_bypass_category_chooser() {
        for label in ["loop", "one-shot", "one_shot", "oneshot"] {
            let category = infer_tag_category(label).expect("bounded alias");
            assert_eq!(category, TagCategory::PlaybackType, "{label}");
            assert!(profile_category_for_inference(category).is_none());
        }
    }

    #[test]
    fn category_keyboard_cycles_only_extensible_categories() {
        assert_eq!(
            next_extensible_category(TagCategory::SoundType, -1),
            Some(TagCategory::TuningScale)
        );
        assert_eq!(
            next_extensible_category(TagCategory::TuningScale, 1),
            Some(TagCategory::SoundType)
        );
        assert_eq!(next_extensible_category(TagCategory::PlaybackType, 1), None);
    }

    #[test]
    fn source_page_reply_requires_the_captured_identity_and_revision() {
        let context = TagSubmissionContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/samples"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 4,
            revision: 7,
            paths: vec![PathBuf::from("kick.wav")],
        };
        assert!(source_page_reply_is_coherent(
            &context,
            &context.source_id,
            context.lifecycle_generation,
            &context.root_identity,
            context.revision,
            context.revision,
        ));
        assert!(!source_page_reply_is_coherent(
            &context,
            &context.source_id,
            context.lifecycle_generation,
            &RootIdentity::from_bytes(b"other".to_vec()),
            context.revision,
            context.revision,
        ));
        assert!(!source_page_reply_is_coherent(
            &context,
            &context.source_id,
            context.lifecycle_generation,
            &context.root_identity,
            context.revision + 1,
            context.revision + 1,
        ));
        assert!(!source_page_reply_is_coherent(
            &context,
            &context.source_id,
            context.lifecycle_generation,
            &context.root_identity,
            context.revision,
            context.revision + 1,
        ));
    }
}

fn should_clear_after_admission(submitted_generation: u64, current_generation: u64) -> bool {
    submitted_generation == current_generation
}
