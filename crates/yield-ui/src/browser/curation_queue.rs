//! Bounded, revision-fenced per-file facts for the browser Curation queue.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{ClickEvent, Context, Window, div, prelude::*, px};
use yield_core::{AudioFile, AudioFileId, CurationQueueMode, CurationQueueScope, CurationTagFacts};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogStatus, CatalogSubmitError,
    MAX_CURATION_TAG_FACTS_PATH_BYTES, SOURCE_CURATION_TAG_FACTS_MAX_PATHS, SourceCurationTagFacts,
};

use super::{Browser, catalog, catalog_metadata::CatalogMetadataContext, curation};
use crate::theme::color;
use yield_design::Theme;

const MAX_BATCH_RAW_PATH_BYTES: usize = MAX_CURATION_TAG_FACTS_PATH_BYTES / 2;

#[derive(Clone, Debug)]
struct PendingBatch {
    context: CatalogMetadataContext,
    request_id: u64,
    start: usize,
    paths: Vec<PathBuf>,
}

/// Source-scoped queue read state. Facts are published to the model only after every batch
/// completed for the same audited source context.
#[derive(Default)]
pub(super) struct CurationQueueState {
    context: Option<CatalogMetadataContext>,
    pending: Option<PendingBatch>,
    facts: Vec<Option<CurationTagFacts>>,
    next_index: usize,
    ready: bool,
    next_request_id: u64,
    focus: Option<(Option<AudioFileId>, Option<usize>)>,
    error: Option<String>,
    scope_menu_open: bool,
}

impl CurationQueueState {
    fn reset(&mut self) {
        self.context = None;
        self.pending = None;
        self.facts.clear();
        self.next_index = 0;
        self.ready = false;
        self.focus = None;
        self.error = None;
        self.scope_menu_open = false;
    }

    fn reset_for_context(&mut self, context: CatalogMetadataContext, entry_count: usize) {
        self.context = Some(context);
        self.pending = None;
        self.facts = vec![None; entry_count];
        self.next_index = 0;
        self.ready = false;
        self.error = None;
        self.scope_menu_open = false;
    }

    fn next_batch(&mut self, entries: &[AudioFile]) -> Result<Option<PendingBatch>, String> {
        let Some(context) = self.context.clone() else {
            return Ok(None);
        };
        if self.next_index >= entries.len() {
            return Ok(None);
        }
        let mut paths = Vec::new();
        let mut raw_bytes = 0usize;
        for entry in entries[self.next_index..]
            .iter()
            .take(SOURCE_CURATION_TAG_FACTS_MAX_PATHS)
        {
            let path_bytes = entry.relative_path.as_os_str().as_encoded_bytes().len();
            if raw_bytes.saturating_add(path_bytes) > MAX_BATCH_RAW_PATH_BYTES {
                break;
            }
            raw_bytes += path_bytes;
            paths.push(entry.relative_path.clone());
        }
        if paths.is_empty() {
            return Err(String::from(
                "Curation queue path exceeds the bounded read limit",
            ));
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        Ok(Some(PendingBatch {
            context,
            request_id: self.next_request_id,
            start: self.next_index,
            paths,
        }))
    }
}

fn contexts_match(left: &CatalogMetadataContext, right: &CatalogMetadataContext) -> bool {
    left.source_id == right.source_id
        && left.lifecycle_generation == right.lifecycle_generation
        && left.root == right.root
        && left.root_identity == right.root_identity
        && left.directory_generation == right.directory_generation
        && left.revision == right.revision
        && Arc::ptr_eq(&left.entry_indices, &right.entry_indices)
}

fn current_context(browser: &Browser) -> Option<CatalogMetadataContext> {
    let activation = browser.catalog_activation.as_ref()?;
    if !activation.audit_finished || activation.audit_error.is_some() {
        return None;
    }
    let context = browser.catalog_metadata.context()?;
    let key = curation::CurationLifecycleKey {
        source_id: activation.source_id.clone(),
        lifecycle_generation: activation.lifecycle_generation,
    };
    (context.source_id == key.source_id
        && context.lifecycle_generation == key.lifecycle_generation
        && context.root == activation.root
        && context.entry_indices.len() == browser.entries.len()
        && browser.curation_state.authority_matches(
            &key,
            &context.root_identity,
            context.directory_generation,
            context.revision,
        ))
    .then_some(context)
}

/// Preserve visible focus before a passive projection invalidates queue facts.
pub(super) fn preserve_focus_for_projection(browser: &mut Browser) {
    if browser.view_model.curation_queue().enabled && browser.curation_queue.focus.is_none() {
        browser.curation_queue.focus = Some((
            browser.browser_focus_id(),
            browser.selected_visible_position(),
        ));
    }
}

/// Clear the fact lane when an activation changes source or lifecycle.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.curation_queue.reset();
    browser
        .view_model
        .invalidate_curation_tag_facts(&browser.entries);
}

/// Resume or reapply an enabled queue after the browser replaces its entry projection.
pub(super) fn reinstall_after_projection(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.view_model.curation_queue().enabled {
        browser.start_source_registry_poll(cx);
    }
}

/// Invalidate an active queue after a curation receipt changes source revision or metadata.
pub(super) fn invalidate_after_curation(browser: &mut Browser, cx: &mut Context<Browser>) {
    if !browser.view_model.curation_queue().enabled {
        return;
    }
    preserve_focus_for_projection(browser);
    browser.curation_queue.context = None;
    browser.curation_queue.pending = None;
    browser.curation_queue.ready = false;
    browser.curation_queue.error = None;
    browser
        .view_model
        .invalidate_curation_tag_facts(&browser.entries);
    browser.refresh_collection_filter_counts();
    browser.start_source_registry_poll(cx);
}

/// Change the active source's independent Curation enable state.
pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    let Some(index) = browser.active_source else {
        return;
    };
    let mut mode = browser.source_sessions[index].curation_queue_mode;
    mode.enabled = !mode.enabled;
    set_mode(browser, index, mode, cx);
}

/// Open or close the Curation scope choices.
pub(super) fn toggle_scope_menu(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.active_source.is_none() {
        return;
    }
    browser.curation_queue.scope_menu_open = !browser.curation_queue.scope_menu_open;
    cx.notify();
}

/// Return whether the active source's scope choices are visible.
pub(super) fn scope_menu_open(browser: &Browser) -> bool {
    browser.curation_queue.scope_menu_open
}

/// Change the active source's Curation dropdown without changing its enable state.
pub(super) fn set_scope(
    browser: &mut Browser,
    scope: CurationQueueScope,
    cx: &mut Context<Browser>,
) {
    let Some(index) = browser.active_source else {
        return;
    };
    let mut mode = browser.source_sessions[index].curation_queue_mode;
    mode.scope = scope;
    browser.curation_queue.scope_menu_open = false;
    cx.notify();
    set_mode(browser, index, mode, cx);
}

/// Keep the two controls operable when macOS keyboard focus is on their AX handles.
pub(super) fn handle_key_down(
    browser: &mut Browser,
    key: &str,
    is_held: bool,
    has_modifiers: bool,
    window: &mut Window,
    cx: &mut Context<Browser>,
) -> bool {
    if has_modifiers || browser.active_source.is_none() {
        return false;
    }
    let toggle_focused = browser
        .ax_focus(super::accessibility::CURATION_QUEUE_TOGGLE)
        .is_some_and(|focus| focus.is_focused(window));
    let scope_focus = browser.ax_focus(super::accessibility::CURATION_QUEUE_SCOPE);
    let scope_focused = scope_focus
        .as_ref()
        .is_some_and(|focus| focus.is_focused(window));
    let choices = [
        CurationQueueScope::All,
        CurationQueueScope::Ratings,
        CurationQueueScope::Tags,
    ];
    let choice_focused = browser
        .curation_queue
        .scope_menu_open
        .then(|| {
            choices.iter().enumerate().find_map(|(index, _)| {
                browser
                    .ax_focus(&super::accessibility::curation_queue_choice_id(index))
                    .filter(|focus| focus.is_focused(window))
                    .map(|_| index)
            })
        })
        .flatten();
    if key == "escape" && browser.curation_queue.scope_menu_open {
        browser.curation_queue.scope_menu_open = false;
        if let Some(focus) = scope_focus {
            window.focus(&focus);
        }
        cx.notify();
        return true;
    }
    if is_held {
        return toggle_focused || scope_focused || choice_focused.is_some();
    }
    if matches!(key, "enter" | "space") {
        if toggle_focused {
            toggle(browser, cx);
            return true;
        }
        if let Some(index) = choice_focused {
            set_scope(browser, choices[index], cx);
            if let Some(focus) = scope_focus {
                window.focus(&focus);
            }
            return true;
        }
        if scope_focused {
            toggle_scope_menu(browser, cx);
            return true;
        }
    }
    if matches!(key, "up" | "arrowup" | "down" | "arrowdown")
        && (scope_focused || choice_focused.is_some())
    {
        if !browser.curation_queue.scope_menu_open {
            browser.curation_queue.scope_menu_open = true;
        }
        let down = matches!(key, "down" | "arrowdown");
        let index = choice_focused
            .map(|index| {
                if down {
                    (index + 1) % choices.len()
                } else {
                    (index + choices.len() - 1) % choices.len()
                }
            })
            .unwrap_or(if down { 0 } else { choices.len() - 1 });
        if let Some(focus) =
            browser.ax_focus(&super::accessibility::curation_queue_choice_id(index))
        {
            window.focus(&focus);
        }
        cx.notify();
        return true;
    }
    // Focused queue controls must not fall through to sample audition or curation shortcuts.
    (toggle_focused || scope_focused || choice_focused.is_some()) && key != "tab"
}

fn set_mode(
    browser: &mut Browser,
    index: usize,
    mode: CurationQueueMode,
    cx: &mut Context<Browser>,
) {
    if browser.source_sessions[index].curation_queue_mode == mode {
        return;
    }
    preserve_focus_for_projection(browser);
    let focus = browser.curation_queue.focus.take().unwrap_or_else(|| {
        (
            browser.browser_focus_id(),
            browser.selected_visible_position(),
        )
    });
    browser.source_sessions[index].curation_queue_mode = mode;
    if !mode.enabled || !browser.curation_queue.ready {
        browser
            .view_model
            .invalidate_curation_tag_facts(&browser.entries);
    }
    browser
        .view_model
        .set_curation_queue(&browser.entries, mode);
    if mode.enabled {
        if browser.curation_queue.ready && !browser.view_model.curation_queue_pending() {
            browser.reset_browser_focus_state();
            browser.restore_projection_focus(focus.0, focus.1, cx);
        } else {
            browser.curation_queue.focus = Some(focus);
            browser.start_source_registry_poll(cx);
        }
    } else {
        browser.curation_queue.reset();
        browser.reset_browser_focus_state();
        browser.restore_projection_focus(focus.0, focus.1, cx);
    }
    browser.refresh_collection_filter_counts();
    browser.reconcile_selection();
    browser.context_menu = None;
    browser.invalidate_file_handoff();
    cx.notify();
}

/// Return whether another source-registry poll is required to complete the active queue.
pub(super) fn has_pending(browser: &Browser) -> bool {
    browser.view_model.curation_queue().enabled
        && browser.curation_queue.error.is_none()
        && browser
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| activation.audit_error.is_none())
        && (!browser.curation_queue.ready
            || browser.curation_queue.pending.is_some()
            || browser.view_model.curation_queue_pending())
}

/// Return an honest queue status for the sidebar or browser empty-state label.
pub(super) fn status_label(browser: &Browser) -> Option<String> {
    if !browser.view_model.curation_queue().enabled {
        return None;
    }
    if browser.catalog_activation.is_none() {
        return Some(String::from(
            "Curation queue needs an active source catalog",
        ));
    }
    if browser
        .catalog_activation
        .as_ref()
        .is_some_and(|activation| activation.audit_error.is_some())
    {
        return Some(String::from(
            "Curation queue is waiting for a valid catalog audit",
        ));
    }
    browser.curation_queue.error.clone().or_else(|| {
        (!browser.curation_queue.ready).then_some(String::from("Loading Curation queue…"))
    })
}

pub(super) fn scope_label(scope: CurationQueueScope) -> &'static str {
    match scope {
        CurationQueueScope::All => "All",
        CurationQueueScope::Ratings => "Rate",
        CurationQueueScope::Tags => "Tags",
    }
}

/// Render source-scoped Curation enable and scope controls.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let mode = browser.view_model.curation_queue();
    let active_source = browser.active_source.is_some();
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let warning = color(theme.colors.warning);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .pb(px(theme.spacing.control_gap))
        .border_t(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .child(
            div()
                .flex()
                .items_center()
                .h(px(theme.spacing.row_height))
                .px(px(theme.spacing.panel_padding))
                .gap(px(theme.spacing.control_gap))
                .text_size(px(theme.typography.small_size))
                .child(
                    div()
                        .id("curation-queue-toggle")
                        .when_some(
                            browser.ax_focus(super::accessibility::CURATION_QUEUE_TOGGLE),
                            |element, focus| element.track_focus(&focus),
                        )
                        .h(px(theme.spacing.row_height))
                        .items_center()
                        .cursor_pointer()
                        .text_color(color(if mode.enabled {
                            theme.colors.accent
                        } else {
                            theme.colors.text_muted
                        }))
                        .hover(move |style| style.bg(hover))
                        .active(move |style| style.bg(pressed))
                        .when(!active_source, |element| element.opacity(0.45))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            if active_source {
                                toggle(view, cx);
                            }
                        }))
                        .child("CURATE")
                        .when_some(
                            browser.ax_marker(super::accessibility::CURATION_QUEUE_TOGGLE),
                            |element, marker| element.child(marker),
                        ),
                )
                .child(
                    div()
                        .id("curation-queue-scope")
                        .when_some(
                            browser.ax_focus(super::accessibility::CURATION_QUEUE_SCOPE),
                            |element, focus| element.track_focus(&focus),
                        )
                        .flex_1()
                        .min_w_0()
                        .h(px(theme.spacing.row_height))
                        .items_center()
                        .cursor_pointer()
                        .text_color(color(theme.colors.text_primary))
                        .hover(move |style| style.bg(hover))
                        .active(move |style| style.bg(pressed))
                        .when(!active_source, |element| element.opacity(0.45))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            if active_source {
                                toggle_scope_menu(view, cx);
                            }
                        }))
                        .child(format!("{} ▾", scope_label(mode.scope)))
                        .when_some(
                            browser.ax_marker(super::accessibility::CURATION_QUEUE_SCOPE),
                            |element, marker| element.child(marker),
                        ),
                ),
        )
        .when(browser.curation_queue.scope_menu_open, |element| {
            element.child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(theme.spacing.panel_padding))
                    .children(
                        [
                            CurationQueueScope::All,
                            CurationQueueScope::Ratings,
                            CurationQueueScope::Tags,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, scope)| {
                            div()
                                .id(("curation-queue-choice", index))
                                .when_some(
                                    browser.ax_focus(
                                        &super::accessibility::curation_queue_choice_id(index),
                                    ),
                                    |element, focus| element.track_focus(&focus),
                                )
                                .h(px(theme.spacing.row_height))
                                .items_center()
                                .cursor_pointer()
                                .text_size(px(theme.typography.small_size))
                                .text_color(color(if mode.scope == scope {
                                    theme.colors.accent
                                } else {
                                    theme.colors.text_primary
                                }))
                                .hover(move |style| style.bg(hover))
                                .active(move |style| style.bg(pressed))
                                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                    set_scope(view, scope, cx);
                                    if let Some(focus) =
                                        view.ax_focus(super::accessibility::CURATION_QUEUE_SCOPE)
                                    {
                                        window.focus(&focus);
                                    }
                                }))
                                .child(scope_label(scope))
                                .when_some(
                                    browser.ax_marker(
                                        &super::accessibility::curation_queue_choice_id(index),
                                    ),
                                    |element, marker| element.child(marker),
                                )
                        }),
                    ),
            )
        })
        .children(status_label(browser).into_iter().map(|message| {
            div()
                .px(px(theme.spacing.panel_padding))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(theme.typography.small_size))
                .text_color(warning)
                .child(message)
        }))
}

/// Submit at most one bounded path batch or publish a complete in-memory queue projection.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if !browser.view_model.curation_queue().enabled {
        return false;
    }
    let Some(context) = current_context(browser) else {
        return false;
    };
    if browser
        .curation_queue
        .context
        .as_ref()
        .is_none_or(|previous| !contexts_match(previous, &context))
    {
        preserve_focus_for_projection(browser);
        browser
            .curation_queue
            .reset_for_context(context.clone(), browser.entries.len());
        browser
            .view_model
            .invalidate_curation_tag_facts(&browser.entries);
        browser.refresh_collection_filter_counts();
    }
    if browser.curation_queue.ready {
        if browser.view_model.curation_queue_pending() {
            let facts = browser
                .curation_queue
                .facts
                .iter()
                .copied()
                .collect::<Option<Vec<_>>>();
            if let Some(facts) = facts {
                if browser
                    .view_model
                    .apply_curation_tag_facts(&browser.entries, facts)
                {
                    browser.refresh_collection_filter_counts();
                    browser.reconcile_selection();
                    return true;
                }
            }
            browser.curation_queue.ready = false;
            browser.curation_queue.next_index = 0;
            browser.curation_queue.facts = vec![None; browser.entries.len()];
        }
        return false;
    }
    if browser.curation_queue.pending.is_some() || browser.curation_queue.error.is_some() {
        return false;
    }
    if browser.curation_queue.next_index == browser.entries.len() {
        let facts = browser
            .curation_queue
            .facts
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>();
        let Some(facts) = facts else {
            browser.curation_queue.error =
                Some(String::from("Curation queue facts are incomplete"));
            return true;
        };
        if !browser
            .view_model
            .apply_curation_tag_facts(&browser.entries, facts)
        {
            browser.curation_queue.error = Some(String::from("Curation queue projection changed"));
            return true;
        }
        browser.curation_queue.ready = true;
        browser.refresh_collection_filter_counts();
        browser.reconcile_selection();
        let focus = browser.curation_queue.focus.take();
        if let Some((id, position)) = focus {
            browser.reset_browser_focus_state();
            browser.restore_projection_focus(id, position, cx);
        }
        return true;
    }
    if browser.curation_operation_pending() {
        return false;
    }
    let status = cx
        .has_global::<super::source_registry::SourceRegistryGlobal>()
        .then(|| {
            cx.global::<super::source_registry::SourceRegistryGlobal>()
                .catalog_status()
        });
    if matches!(
        status,
        Some(CatalogStatus::Unavailable { .. } | CatalogStatus::Closed)
    ) {
        browser.curation_queue.error = Some(String::from(
            "Curation queue is waiting for the source catalog",
        ));
        return true;
    }
    let batch = match browser.curation_queue.next_batch(&browser.entries) {
        Ok(Some(batch)) => batch,
        Ok(None) => return false,
        Err(error) => {
            browser.curation_queue.error = Some(error);
            return true;
        }
    };
    let command = CatalogCommand::RequestCurationTagFacts {
        source_id: context.source_id.clone(),
        lifecycle_generation: context.lifecycle_generation,
        expected_revision: context.revision,
        request_id: batch.request_id,
        relative_paths: batch.paths.clone(),
    };
    browser.curation_queue.pending = Some(batch);
    match catalog::submit_catalog_command(command, cx) {
        Ok(()) => true,
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
            browser.curation_queue.pending = None;
            false
        }
        Err(error) => {
            browser.curation_queue.pending = None;
            browser.curation_queue.error = Some(format!("Curation queue unavailable: {error}"));
            true
        }
    }
}

/// Consume a tag-facts completion only for its exact pending context and path batch.
pub(super) fn apply_catalog_event(browser: &mut Browser, event: &CatalogEvent) -> bool {
    match event {
        CatalogEvent::CurationTagFactsReady {
            source_id,
            lifecycle_generation,
            root_identity,
            directory_generation,
            revision,
            request_id,
            requested_paths,
            entries,
        } => {
            let Some(pending) = browser.curation_queue.pending.clone() else {
                return false;
            };
            let Some(current) = current_context(browser) else {
                return false;
            };
            if !contexts_match(&pending.context, &current)
                || pending.context.source_id != *source_id
                || pending.context.lifecycle_generation != *lifecycle_generation
                || pending.context.root_identity != *root_identity
                || pending.context.directory_generation != *directory_generation
                || pending.context.revision != *revision
                || pending.request_id != *request_id
                || pending.paths != *requested_paths
            {
                return false;
            }
            let Some(facts) = facts_in_requested_order(requested_paths, entries) else {
                browser.curation_queue.error = Some(String::from(
                    "Curation queue received an incomplete source batch",
                ));
                browser.curation_queue.pending = None;
                return true;
            };
            for (offset, fact) in facts.into_iter().enumerate() {
                browser.curation_queue.facts[pending.start + offset] = Some(fact);
            }
            browser.curation_queue.next_index += pending.paths.len();
            browser.curation_queue.pending = None;
            true
        }
        CatalogEvent::CurationTagFactsFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
            ..
        } => {
            let Some(pending) = browser.curation_queue.pending.as_ref() else {
                return false;
            };
            if pending.context.source_id != *source_id
                || pending.context.lifecycle_generation != *lifecycle_generation
                || pending.context.revision != *expected_revision
                || pending.request_id != *request_id
            {
                return false;
            }
            browser.curation_queue.pending = None;
            browser.curation_queue.error = Some(format!("Curation queue unavailable: {error}"));
            true
        }
        _ => false,
    }
}

fn facts_in_requested_order(
    paths: &[PathBuf],
    entries: &[SourceCurationTagFacts],
) -> Option<Vec<CurationTagFacts>> {
    if paths.len() != entries.len() {
        return None;
    }
    let unique = entries
        .iter()
        .map(|entry| &entry.relative_path)
        .collect::<std::collections::HashSet<_>>();
    if unique.len() != entries.len() {
        return None;
    }
    paths
        .iter()
        .map(|path| {
            let entry = entries.iter().find(|entry| entry.relative_path == *path)?;
            Some(CurationTagFacts::from_flags(
                entry.has_any_tag,
                entry.has_descriptive_tag,
                entry.has_playback_type_tag,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use yield_core::AudioFile;
    use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
    use yield_session::SourceCurationTagFacts;

    use super::{
        CatalogMetadataContext, CurationQueueState, contexts_match, facts_in_requested_order,
    };

    fn context(indices: Arc<HashMap<PathBuf, usize>>) -> CatalogMetadataContext {
        CatalogMetadataContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/source"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 4,
            revision: 5,
            entry_indices: indices,
        }
    }

    fn file(index: usize) -> AudioFile {
        let name = format!("{index}.wav");
        AudioFile {
            path: PathBuf::from(format!("/source/{name}")),
            relative_path: PathBuf::from(name),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn batches_stop_at_64_paths_and_retain_exact_context() {
        let files = (0..130).map(file).collect::<Vec<_>>();
        let indices = Arc::new(
            files
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.relative_path.clone(), index))
                .collect(),
        );
        let original = context(indices);
        let mut state = CurationQueueState::default();
        state.reset_for_context(original.clone(), files.len());
        let first = state.next_batch(&files).unwrap().unwrap();
        assert_eq!(first.paths.len(), 64);
        assert_eq!(first.start, 0);
        assert!(contexts_match(&first.context, &original));
        state.next_index += first.paths.len();
        let second = state.next_batch(&files).unwrap().unwrap();
        assert_eq!(second.paths.len(), 64);
        assert_eq!(second.start, 64);
        assert_ne!(first.request_id, second.request_id);

        let replacement = context(Arc::new((*original.entry_indices).clone()));
        assert!(!contexts_match(&original, &replacement));
    }

    #[test]
    fn result_facts_require_one_distinct_entry_for_every_requested_path() {
        let paths = vec![PathBuf::from("a.wav"), PathBuf::from("b.wav")];
        let complete = SourceCurationTagFacts {
            relative_path: paths[0].clone(),
            has_any_tag: true,
            has_descriptive_tag: true,
            has_playback_type_tag: true,
        };
        let empty = SourceCurationTagFacts {
            relative_path: paths[1].clone(),
            has_any_tag: false,
            has_descriptive_tag: false,
            has_playback_type_tag: false,
        };
        let facts = facts_in_requested_order(&paths, &[empty.clone(), complete.clone()]).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(
            facts[0],
            yield_core::CurationTagFacts::from_flags(true, true, true)
        );
        assert_eq!(
            facts[1],
            yield_core::CurationTagFacts::from_flags(false, false, false)
        );
        assert!(facts_in_requested_order(&paths, &[complete.clone()]).is_none());
        assert!(facts_in_requested_order(&paths, &[complete.clone(), complete]).is_none());
    }
}
