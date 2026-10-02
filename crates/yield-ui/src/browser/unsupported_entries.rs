//! Bounded retained-index modal. All database/filesystem work belongs to the catalog owner.
use super::{Browser, catalog, source_registry::SourceRegistryGlobal};
use crate::theme::color;
use gpui::{
    ClickEvent, Context, FocusHandle, KeyDownEvent, MouseDownEvent, Pixels, ScrollHandle, Size,
    Window, div, prelude::*, px,
};
use std::path::Path;
use yield_design::Theme;
use yield_library::sample_sources::{SourceId, SourceIndexClassification, SourceIndexEntry};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogIndexInspection, CatalogIndexPage, CatalogIndexPageCursor,
    CatalogSubmitError, IndexInspectionCancellation, SOURCE_INDEX_PAGE_MAX_ENTRIES,
};

const HISTORY_LIMIT: usize = 8;
const PAGE_BYTES: usize = 128 * 1024;
const DISPLAY_BYTES: usize = 1024;
const MODAL_ID: &str = "unsupported-entries-modal";
const LIST_ID: &str = "unsupported-entries-list";
const STATUS_ID: &str = "unsupported-entries-status";
const CLOSE_ID: &str = "unsupported-entries-close";

gpui::actions!(
    unsupported_entries,
    [
        OpenUnsupportedFiles,
        CloseUnsupportedFiles,
        InspectUnsupportedFile
    ]
);

#[cfg(target_os = "macos")]
mod accessibility;
mod model;
mod presentation;
#[cfg(test)]
mod tests;
mod view;
#[cfg(target_os = "macos")]
pub(super) use accessibility::{accessibility_controls, accessibility_focus, handle_accessibility};
use model::{Binding, Model, eligible};
#[cfg(test)]
use model::{PendingInspection, bounded_page};
use presentation::{display_path, row_description};
pub(super) use view::render;

/// Focus and bounded state for this source-owned read-only modal.
pub(super) struct UnsupportedEntriesState {
    model: Model,
    open: bool,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
}
impl UnsupportedEntriesState {
    /// Construct a closed modal with browser-owned focus/scroll handles.
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        Self {
            model: Model::default(),
            open: false,
            focus: cx.focus_handle(),
            previous_focus: None,
            scroll: ScrollHandle::new(),
        }
    }
    /// Whether the modal owns browser input.
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}
fn cancel_binding(binding: Option<Binding>, cx: &mut Context<Browser>) {
    if let Some(b) = binding
        && cx.has_global::<SourceRegistryGlobal>()
    {
        cx.global::<SourceRegistryGlobal>()
            .cancel_catalog_index_query(&b.source_id, b.generation, b.query_id);
    }
}
/// Invalidate all projections when the browser's registered source changes or retires.
pub(super) fn cancel_for_source_change(browser: &mut Browser, cx: &mut Context<Browser>) {
    cancel_binding(browser.unsupported_entries.model.cancel(), cx);
    if browser.unsupported_entries.open {
        browser.unsupported_entries.model.status =
            "Source changed or unavailable. Restart the list.";
    }
}
/// Retry only non-admitted bounded commands after queue pressure; never create an extra request.
pub(super) fn retry(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if !browser.unsupported_entries.open {
        return false;
    }
    let model = &mut browser.unsupported_entries.model;
    if let Some(command) = model.page_command() {
        match catalog::submit_catalog_command(command, cx) {
            Ok(()) => model.pending.as_mut().unwrap().admitted = true,
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => return false,
            Err(_) => {
                model.pending = None;
                model.status = "Source catalog unavailable. Restart after recovery.";
                return true;
            }
        }
    }
    if let Some(command) = model.inspection_command() {
        match catalog::submit_catalog_command(command, cx) {
            Ok(()) => model.inspection_pending.as_mut().unwrap().admitted = true,
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => return false,
            Err(_) => {
                if let Some(p) = model.inspection_pending.take() {
                    p.cancellation.cancel();
                }
                model.status = "Owner inspection unavailable. Restart after recovery.";
                return true;
            }
        }
    }
    if model
        .inspection
        .as_ref()
        .is_some_and(|inspection| !inspection.is_live())
    {
        model.inspection = None;
        model.status = "Inspection canceled or stale. Restart the list.";
        return true;
    }
    if model
        .inspection_pending
        .as_ref()
        .is_some_and(|p| p.cancellation.is_canceled())
    {
        model.inspection_pending = None;
        model.status = "Inspection canceled or superseded. Retry or restart.";
        return true;
    }
    false
}
/// Consume matching owner events; late or foreign events cannot revive a closed/source-changed modal.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &mut CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    if !matches!(
        event,
        CatalogEvent::IndexPageReady { .. }
            | CatalogEvent::IndexPageFailed { .. }
            | CatalogEvent::IndexInspectionCompleted { .. }
    ) {
        return false;
    }
    if browser.unsupported_entries.open && browser.unsupported_entries.model.apply(event) {
        cx.notify();
    }
    true
}
impl Browser {
    /// Open the read-only modal for the already registered active source, without registering roots.
    pub(super) fn open_unsupported_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shortcut_help.is_open() {
            self.toggle_shortcut_help(window, cx);
        }
        if self.feedback_issue.open {
            return;
        }
        if !self.unsupported_entries.open {
            self.unsupported_entries.previous_focus = window.focused(cx);
            self.cancel_file_drag_with_window(window, cx);
            self.cancel_waveform_gestures(window, cx);
            self.cancel_volume_drag(window, cx);
            self.unsupported_entries.open = true;
        }
        window.focus(&self.unsupported_entries.focus);
        self.restart_unsupported_files(cx);
        cx.notify();
    }
    /// Close and restore focus, canceling owner work and dropping all page/descriptor custody.
    pub(super) fn close_unsupported_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cancel_binding(self.unsupported_entries.model.cancel(), cx);
        self.unsupported_entries.open = false;
        if let Some(previous) = self.unsupported_entries.previous_focus.take() {
            window.focus(&previous);
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }
    /// Restart at the first retained page with a new cancellable query.
    pub(super) fn restart_unsupported_files(&mut self, cx: &mut Context<Self>) {
        cancel_binding(self.unsupported_entries.model.cancel(), cx);
        if let Some(activation) = self.catalog_activation.as_ref().filter(|a| a.registered) {
            self.unsupported_entries.model.begin(
                activation.source_id.clone(),
                activation.lifecycle_generation,
            );
            retry(self, cx);
            self.start_source_registry_poll(cx);
        } else {
            self.unsupported_entries.model.status =
                "No registered source catalog available. Select a source first.";
        }
        cx.notify();
    }
    fn inspect_unsupported_file(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.unsupported_entries.model.selected
            && self.unsupported_entries.model.prepare_inspection(row)
        {
            retry(self, cx);
            self.start_source_registry_poll(cx);
            cx.notify();
        }
    }
    /// Consume every background key while open; Escape restores the prior focus.
    pub(super) fn handle_unsupported_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.unsupported_entries.open {
            return false;
        }
        if !event.is_held {
            match event.keystroke.key.as_str() {
                "escape" => self.close_unsupported_files(window, cx),
                "enter" => self.inspect_unsupported_file(cx),
                "up" | "arrowup" | "down" | "arrowdown" => {
                    let model = &mut self.unsupported_entries.model;
                    let count = model.page.as_ref().map_or(0, |p| p.entries.len());
                    if count > 0 {
                        let current = model.selected.unwrap_or(0);
                        let row = if event.keystroke.key == "up" || event.keystroke.key == "arrowup"
                        {
                            current.saturating_sub(1)
                        } else {
                            (current + 1).min(count - 1)
                        };
                        model.select_row(row);
                        if let Some(row) = model.selected {
                            self.unsupported_entries.scroll.scroll_to_item(row);
                        }
                        cx.notify();
                    }
                }
                _ => {}
            }
        }
        cx.stop_propagation();
        true
    }
    /// Typed open action and visible toolbar route.
    pub(super) fn open_unsupported_action(
        &mut self,
        _: &OpenUnsupportedFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_unsupported_files(window, cx);
    }
    /// Typed close action, with cancellation and focus restoration.
    pub(super) fn close_unsupported_action(
        &mut self,
        _: &CloseUnsupportedFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_unsupported_files(window, cx);
    }
    /// Typed inspection action; it never performs native reveal.
    pub(super) fn inspect_unsupported_action(
        &mut self,
        _: &InspectUnsupportedFile,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inspect_unsupported_file(cx);
    }
    /// Visible toolbar entry point.
    pub(super) fn open_unsupported_from_toolbar(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_unsupported_files(window, cx);
    }
}
