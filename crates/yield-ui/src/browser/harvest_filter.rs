//! Source-scoped consumer of the actual coherent catalog Harvest mask.
use super::{
    Browser, catalog_metadata::CatalogMetadataContext, source_registry::SourceRegistryGlobal,
};
use gpui::Context;
use std::sync::Arc;
use yield_core::{AudioFileId, TagFilterMask};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogHarvestFilter, CatalogStatus, CatalogSubmitError,
    HarvestFactsCancellation, HarvestFilterChoice, SourceHarvestFilterRequest,
};
mod controls;
mod read_lane;
mod status;
pub(super) use controls::{handle_key_down, render};
pub(super) use read_lane::{apply_catalog_event, invalidate_if_stale, poll};
use status::failure_label;

/// Choice and independent enable bit retained by each source session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct HarvestMode {
    pub(super) enabled: bool,
    pub(super) choice: HarvestFilterChoice,
}
impl HarvestMode {
    fn filtering(self) -> bool {
        self.enabled && self.choice != HarvestFilterChoice::All
    }
}
struct Pending {
    request: SourceHarvestFilterRequest,
    context: CatalogMetadataContext,
    cancellation: HarvestFactsCancellation,
    started: std::time::Instant,
}
/// One active source's bounded mask lane; no source/global ownership is manufactured here.
#[derive(Default)]
pub(super) struct HarvestRuntime {
    pending: Option<Pending>,
    ready: Option<(CatalogMetadataContext, CatalogHarvestFilter)>,
    error: Option<String>,
    next_request: u64,
    focus: Option<(Option<AudioFileId>, Option<usize>)>,
    pub(super) menu_open: bool,
}
impl HarvestRuntime {
    fn cancel(&mut self) {
        if let Some(p) = self.pending.take() {
            p.cancellation.cancel();
        }
        self.ready.take();
        self.focus.take();
        self.error.take();
        self.menu_open = false;
    }
}
/// Current source-scoped mode, defaulting to disabled All without a source.
pub(super) fn mode(browser: &Browser) -> HarvestMode {
    browser
        .active_source
        .and_then(|i| browser.source_sessions.get(i))
        .map(|session| session.harvest_mode)
        .unwrap_or_default()
}
fn matches(a: &CatalogMetadataContext, b: &CatalogMetadataContext) -> bool {
    a.source_id == b.source_id
        && a.lifecycle_generation == b.lifecycle_generation
        && a.root == b.root
        && a.root_identity == b.root_identity
        && a.directory_generation == b.directory_generation
        && a.revision == b.revision
        && Arc::ptr_eq(&a.entry_indices, &b.entry_indices)
}

fn context(browser: &Browser) -> Option<CatalogMetadataContext> {
    let activation = browser.catalog_activation.as_ref()?;
    if !activation.audit_finished || activation.audit_error.is_some() {
        return None;
    }
    let c = browser.catalog_metadata.context()?;
    let key = super::curation::CurationLifecycleKey {
        source_id: activation.source_id.clone(),
        lifecycle_generation: activation.lifecycle_generation,
    };
    (browser.root.as_ref() == Some(&c.root)
        && browser.active_source.is_some_and(|i| {
            browser
                .source_sessions
                .get(i)
                .is_some_and(|s| s.source_id.as_ref() == Some(&c.source_id))
        })
        && c.source_id == key.source_id
        && c.lifecycle_generation == key.lifecycle_generation
        && c.root == activation.root
        && c.entry_indices.len() == browser.entries.len()
        && browser.curation_state.authority_matches(
            &key,
            &c.root_identity,
            c.directory_generation,
            c.revision,
        ))
    .then_some(c)
}
/// Cancel retained reads and invalidate old masks when the source lifecycle changes.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.harvest_filter.cancel();
    browser
        .view_model
        .invalidate_harvest_filter(&browser.entries);
}
/// Restore this source's independent mode after replacing the projection.
pub(super) fn reinstall_after_projection(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.harvest_filter.cancel();
    if mode(browser).filtering() {
        browser
            .view_model
            .invalidate_harvest_filter(&browser.entries);
        if !browser.view_model.harvest_filter().is_enabled() {
            browser.view_model.begin_harvest_filter(&browser.entries);
        }
        browser.start_source_registry_poll(cx);
    } else if browser.view_model.harvest_filter().is_enabled() {
        browser.view_model.disable_harvest_filter(&browser.entries);
    }
}
/// Toggle the independent enable bit, retaining the selected choice.
pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    let mut m = mode(browser);
    m.enabled = !m.enabled;
    set_mode(browser, m, cx);
}
/// Selecting a reference choice enables the filter and closes the dropdown.
pub(super) fn select(
    browser: &mut Browser,
    choice: HarvestFilterChoice,
    cx: &mut Context<Browser>,
) {
    set_mode(
        browser,
        HarvestMode {
            enabled: true,
            choice,
        },
        cx,
    );
    browser.harvest_filter.menu_open = false;
    cx.notify();
}
/// Close the dropdown without changing source scope or selection.
pub(super) fn close_menu(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.harvest_filter.menu_open {
        browser.harvest_filter.menu_open = false;
        cx.notify();
    }
}
/// Toggle the dropdown only for an active source.
pub(super) fn toggle_menu(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.active_source.is_some() {
        browser.harvest_filter.menu_open = !browser.harvest_filter.menu_open;
        cx.notify();
    }
}
fn set_mode(browser: &mut Browser, m: HarvestMode, cx: &mut Context<Browser>) {
    let Some(index) = browser
        .active_source
        .filter(|i| *i < browser.source_sessions.len())
    else {
        return;
    };
    if mode(browser) == m && browser.harvest_filter.error.is_none() {
        return;
    }
    let focus = (
        browser.browser_focus_id(),
        browser.selected_visible_position(),
    );
    browser.harvest_filter.cancel();
    browser.source_sessions[index].harvest_mode = m;
    if m.filtering() {
        browser.view_model.begin_harvest_filter(&browser.entries);
        browser.harvest_filter.focus = Some(focus);
        browser.start_source_registry_poll(cx);
    } else {
        browser.view_model.disable_harvest_filter(&browser.entries);
        browser.reset_browser_focus_state();
        browser.restore_projection_focus(focus.0, focus.1, cx);
    }
    browser.refresh_collection_filter_counts();
    browser.reconcile_selection();
    browser.context_menu = None;
    browser.invalidate_file_handoff();
    cx.notify();
}
/// Honest pending/error status, never an optimistic New classification.
pub(super) fn status_label(browser: &Browser) -> Option<String> {
    if !mode(browser).filtering() {
        return None;
    }
    browser.harvest_filter.error.clone().or_else(|| {
        browser
            .harvest_filter
            .ready
            .is_none()
            .then(|| "Loading Harvest filter…".into())
    })
}

#[cfg(test)]
mod tests;
