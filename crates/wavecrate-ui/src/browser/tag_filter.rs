#![deny(missing_docs)]
#![deny(warnings)]

//! Runtime source-tag filtering for the browser projection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{ClickEvent, Context, div, prelude::*, px};
use wavecrate_core::{AudioFile, AudioFileId, BrowserViewModel, TagFilterMask, parse_tag_filter};
use wavecrate_design::Theme;
use wavecrate_library::sample_sources::{SourceId, reconciliation::RootIdentity};
use wavecrate_session::{CatalogCommand, CatalogEvent, CatalogStatus, CatalogSubmitError};

use crate::theme::color;

use super::{Browser, catalog, curation};

/// The kind of browser projection that caused a tag-filter refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FilterOrigin {
    /// A user edited or toggled the tag field.
    Explicit,
    /// A passive metadata projection changed while the browser was filtering.
    PassiveMetadata,
    /// A passive playback-type projection changed while the browser was filtering.
    PassivePlayback,
}

impl Default for FilterOrigin {
    fn default() -> Self {
        Self::Explicit
    }
}

impl FilterOrigin {
    fn is_passive(self) -> bool {
        !matches!(self, Self::Explicit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FocusIntent {
    id: Option<AudioFileId>,
    position: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilterContext {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    root_identity: RootIdentity,
    directory_generation: u64,
    revision: u64,
    manifest_len: usize,
    entry_indices: Arc<HashMap<PathBuf, usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingFilter {
    request_id: u64,
    draft_generation: u64,
    context: FilterContext,
}

#[derive(Clone, Debug)]
pub(super) struct ReusableMaskCandidate {
    context: FilterContext,
    draft_generation: u64,
    mask: TagFilterMask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DraftAction {
    Refresh,
    Disable,
    HoldInvalid,
}

/// Runtime state for the desired tag field and its bounded catalog read.
#[derive(Default)]
pub(super) struct TagFilterState {
    desired_enabled: bool,
    requirements: Vec<String>,
    draft_generation: u64,
    input_error: Option<String>,
    status: Option<String>,
    pending: Option<PendingFilter>,
    retry_ready: bool,
    focus_intent: Option<FocusIntent>,
    focus_origin: FilterOrigin,
    ready_context: Option<FilterContext>,
    ready_draft_generation: Option<u64>,
}

impl TagFilterState {
    fn effective_enabled(&self) -> bool {
        self.desired_enabled && self.input_error.is_none() && !self.requirements.is_empty()
    }

    fn filter_waiting(&self) -> bool {
        self.desired_enabled && (self.input_error.is_some() || !self.requirements.is_empty())
    }

    fn bump_generation(&mut self) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
        if self.draft_generation == 0 {
            self.draft_generation = 1;
        }
    }

    fn clear_request(&mut self) {
        self.pending = None;
        self.retry_ready = false;
    }

    fn clear_ready_context(&mut self) {
        self.ready_context = None;
        self.ready_draft_generation = None;
    }

    fn apply_valid_draft(&mut self, requirements: Vec<String>) -> DraftAction {
        self.bump_generation();
        self.status = None;
        self.input_error = None;
        self.requirements = requirements;
        if self.requirements.is_empty() {
            self.retry_ready = false;
            self.clear_ready_context();
            DraftAction::Disable
        } else {
            self.desired_enabled = true;
            DraftAction::Refresh
        }
    }

    fn apply_invalid_draft(&mut self, error: String) -> DraftAction {
        self.bump_generation();
        self.status = None;
        self.desired_enabled = true;
        self.input_error = Some(error);
        self.retry_ready = false;
        self.clear_ready_context();
        DraftAction::HoldInvalid
    }

    fn toggle_desired(&mut self) -> DraftAction {
        self.bump_generation();
        self.status = None;
        self.desired_enabled = !self.desired_enabled;
        if !self.desired_enabled {
            self.retry_ready = false;
            DraftAction::Disable
        } else if self.input_error.is_some() {
            DraftAction::HoldInvalid
        } else if self.requirements.is_empty() {
            DraftAction::Disable
        } else {
            DraftAction::Refresh
        }
    }

    fn consume_superseded_response(&mut self, response_draft_generation: u64) -> bool {
        if self.pending.is_none() || response_draft_generation == self.draft_generation {
            return false;
        }
        self.pending = None;
        self.retry_ready = self.effective_enabled();
        true
    }

    fn focus_for_new_refresh(&self, browser: &Browser, origin: FilterOrigin) -> FocusIntent {
        if !should_replace_focus(self.focus_intent.as_ref(), self.focus_origin, origin) {
            if let Some(existing) = self.focus_intent.clone() {
                return existing;
            }
        }
        current_focus(browser)
    }
}

/// Handle a valid edit to the bounded tag-filter input.
pub(super) fn on_changed(browser: &mut Browser, content: &str, cx: &mut Context<Browser>) {
    let parsed = parse_tag_filter(content);

    let query = match parsed {
        Ok(query) => query,
        Err(error) => {
            browser.tag_filter.apply_invalid_draft(error.to_string());
            ensure_pending_projection(browser);
            browser.context_menu = None;
            browser.invalidate_file_handoff();
            cx.notify();
            return;
        }
    };
    match browser.tag_filter.apply_valid_draft(query.into_tokens()) {
        DraftAction::Disable => {
            let fallback_focus = current_focus(browser);
            let focus = browser
                .tag_filter
                .focus_intent
                .take()
                .unwrap_or(fallback_focus);
            browser.tag_filter.focus_intent = None;
            browser.tag_filter.clear_ready_context();
            browser.view_model.disable_tag_filter(&browser.entries);
            browser.reset_browser_focus_state();
            browser.refresh_collection_filter_counts();
            browser.reconcile_selection();
            browser.restore_projection_focus(focus.id, focus.position, cx);
            cx.notify();
            return;
        }
        DraftAction::Refresh => begin_refresh(browser, FilterOrigin::Explicit, cx),
        DraftAction::HoldInvalid => unreachable!("valid tag-filter parse cannot remain invalid"),
    }
}

/// Handle a raw input edit rejected by the native input byte bound.
pub(super) fn on_rejected(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_filter.status = Some(format!(
        "Tag filter input exceeds {} bytes",
        wavecrate_core::MAX_TAG_FILTER_INPUT_BYTES
    ));
    cx.notify();
}

/// Toggle the desired tag-filter state while retaining the current field contents.
pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    let action = browser.tag_filter.toggle_desired();
    match action {
        DraftAction::Refresh => begin_refresh(browser, FilterOrigin::Explicit, cx),
        DraftAction::HoldInvalid => {
            ensure_pending_projection(browser);
            browser.context_menu = None;
            browser.invalidate_file_handoff();
            cx.notify();
            return;
        }
        DraftAction::Disable => {}
    }
    if !matches!(action, DraftAction::Disable) {
        return;
    }

    let fallback_focus = current_focus(browser);
    let focus = browser
        .tag_filter
        .focus_intent
        .take()
        .unwrap_or(fallback_focus);
    browser.tag_filter.focus_intent = None;
    browser.tag_filter.clear_ready_context();
    browser.view_model.disable_tag_filter(&browser.entries);
    browser.reset_browser_focus_state();
    browser.refresh_collection_filter_counts();
    browser.reconcile_selection();
    browser.restore_projection_focus(focus.id, focus.position, cx);
    cx.notify();
}

fn begin_refresh(browser: &mut Browser, origin: FilterOrigin, cx: &mut Context<Browser>) {
    let replace_focus = should_replace_focus(
        browser.tag_filter.focus_intent.as_ref(),
        browser.tag_filter.focus_origin,
        origin,
    );
    if replace_focus {
        let focus = browser.tag_filter.focus_for_new_refresh(browser, origin);
        browser.tag_filter.focus_intent = Some(focus);
        browser.tag_filter.focus_origin = origin;
    }
    browser.tag_filter.retry_ready = true;
    browser.tag_filter.status = None;
    browser.tag_filter.clear_ready_context();
    ensure_pending_projection(browser);
    browser.context_menu = None;
    browser.invalidate_file_handoff();
    browser.start_source_registry_poll(cx);
    cx.notify();
}

fn ensure_pending_projection(browser: &mut Browser) {
    if browser.tag_filter.focus_intent.is_none() {
        browser.tag_filter.focus_intent = Some(current_focus(browser));
    }
    browser.view_model.begin_tag_filter(&browser.entries);
}

fn current_focus(browser: &Browser) -> FocusIntent {
    FocusIntent {
        id: browser.browser_focus_id(),
        position: browser.selected_visible_position(),
    }
}

fn should_replace_focus(
    current: Option<&FocusIntent>,
    current_origin: FilterOrigin,
    new_origin: FilterOrigin,
) -> bool {
    current.is_none()
        || (matches!(new_origin, FilterOrigin::Explicit) && current_origin.is_passive())
}

/// Preserve a known focus intent before replacing the entry slice used by the model.
pub(super) fn preserve_focus_for_projection(
    browser: &mut Browser,
    id: Option<AudioFileId>,
    position: Option<usize>,
    origin: FilterOrigin,
) {
    if !browser.tag_filter.filter_waiting() {
        return;
    }
    if should_replace_focus(
        browser.tag_filter.focus_intent.as_ref(),
        browser.tag_filter.focus_origin,
        origin,
    ) {
        browser.tag_filter.focus_intent = Some(FocusIntent { id, position });
        browser.tag_filter.focus_origin = origin;
    }
}

/// Reinstall the desired filter after replacing the browser's entry slice.
pub(super) fn reinstall_after_projection(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.tag_filter.retry_ready = false;
    browser.tag_filter.status = None;
    browser.tag_filter.clear_ready_context();
    if browser.tag_filter.effective_enabled() {
        begin_refresh(browser, FilterOrigin::PassiveMetadata, cx);
    } else if browser.tag_filter.filter_waiting() {
        ensure_pending_projection(browser);
    } else {
        browser.view_model.disable_tag_filter(&browser.entries);
    }
}

/// Invalidate a tag mask after a passive curation revision.
pub(super) fn invalidate_after_curation(
    browser: &mut Browser,
    origin: FilterOrigin,
    cx: &mut Context<Browser>,
) {
    if !browser.tag_filter.effective_enabled() {
        return;
    }
    browser.tag_filter.bump_generation();
    let replace_focus = should_replace_focus(
        browser.tag_filter.focus_intent.as_ref(),
        browser.tag_filter.focus_origin,
        origin,
    );
    if replace_focus {
        let focus = browser.tag_filter.focus_for_new_refresh(browser, origin);
        browser.tag_filter.focus_intent = Some(focus);
        browser.tag_filter.focus_origin = origin;
    }
    browser.tag_filter.retry_ready = true;
    browser.tag_filter.status = None;
    browser.tag_filter.clear_ready_context();
    ensure_pending_projection(browser);
    browser.context_menu = None;
    browser.invalidate_file_handoff();
    browser.start_source_registry_poll(cx);
}

/// Cancel obsolete requests while retaining the desired field across source changes.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.tag_filter.bump_generation();
    browser.tag_filter.clear_request();
    browser.tag_filter.clear_ready_context();
    browser.tag_filter.focus_intent = None;
    browser.tag_filter.status = None;
    browser.invalidate_file_handoff();
    if browser.tag_filter.filter_waiting() {
        browser.view_model.begin_tag_filter(&browser.entries);
    } else {
        browser.view_model.disable_tag_filter(&browser.entries);
    }
}

/// Return whether the effective projection has no authoritative tag scope yet.
pub(super) fn scope_pending(browser: &Browser) -> bool {
    browser.tag_filter.filter_waiting()
        && (browser.tag_filter.pending.is_some()
            || browser.tag_filter.retry_ready
            || browser.view_model.tag_filter().is_pending())
}

/// Return the stable focus captured before a pending projection refresh, when one exists.
pub(super) fn projection_focus(browser: &Browser) -> (Option<AudioFileId>, Option<usize>) {
    if let Some(intent) = browser.tag_filter.focus_intent.as_ref()
        && scope_pending(browser)
    {
        return (intent.id.clone(), intent.position);
    }
    (
        browser.browser_focus_id(),
        browser.selected_visible_position(),
    )
}

/// Capture a validated ready mask for a known metadata-only optimistic update.
pub(super) fn capture_ready_mask(browser: &Browser) -> Option<TagFilterMask> {
    if !browser.tag_filter.effective_enabled() || scope_pending(browser) {
        return None;
    }
    let context = current_context(browser)?;
    if browser.tag_filter.ready_draft_generation != Some(browser.tag_filter.draft_generation)
        || browser
            .tag_filter
            .ready_context
            .as_ref()
            .is_none_or(|ready| !contexts_match(ready, &context))
    {
        return None;
    }
    browser.view_model.tag_filter().mask().cloned()
}

/// Capture a ready mask and its exact audited context before applying a curation receipt.
pub(super) fn capture_reusable_candidate(browser: &Browser) -> Option<ReusableMaskCandidate> {
    let context = current_context(browser)?;
    let mask = capture_ready_mask(browser)?;
    Some(ReusableMaskCandidate {
        context,
        draft_generation: browser.tag_filter.draft_generation,
        mask,
    })
}

/// Validate a previously captured mask against a successful non-tag receipt.
pub(super) fn validate_reusable_candidate(
    browser: &Browser,
    candidate: Option<ReusableMaskCandidate>,
    prior_revision: u64,
    final_revision: u64,
    root_identity: &RootIdentity,
    directory_generation: u64,
    tags_may_change: bool,
    outcome_successful: bool,
) -> Option<TagFilterMask> {
    let candidate = candidate?;
    let metadata = browser.catalog_metadata.context()?;
    let metadata_context = FilterContext {
        source_id: metadata.source_id.clone(),
        lifecycle_generation: metadata.lifecycle_generation,
        root: metadata.root.clone(),
        root_identity: metadata.root_identity.clone(),
        directory_generation: metadata.directory_generation,
        revision: metadata.revision,
        manifest_len: browser.entries.len(),
        entry_indices: Arc::clone(&metadata.entry_indices),
    };
    if !reusable_candidate_matches(
        &candidate,
        &metadata_context,
        browser.tag_filter.draft_generation,
        prior_revision,
        final_revision,
        root_identity,
        directory_generation,
        browser.entries.len(),
        tags_may_change,
        outcome_successful,
    ) {
        return None;
    }
    Some(candidate.mask)
}

fn reusable_candidate_matches(
    candidate: &ReusableMaskCandidate,
    metadata: &FilterContext,
    current_draft_generation: u64,
    prior_revision: u64,
    final_revision: u64,
    root_identity: &RootIdentity,
    directory_generation: u64,
    manifest_len: usize,
    tags_may_change: bool,
    outcome_successful: bool,
) -> bool {
    outcome_successful
        && !tags_may_change
        && final_revision > prior_revision
        && candidate.draft_generation == current_draft_generation
        && candidate.context.revision == prior_revision
        && candidate.context.source_id == metadata.source_id
        && candidate.context.lifecycle_generation == metadata.lifecycle_generation
        && candidate.context.root == metadata.root
        && candidate.context.root_identity == metadata.root_identity
        && candidate.context.directory_generation == metadata.directory_generation
        && candidate.context.revision == metadata.revision
        && candidate.context.manifest_len == manifest_len
        && candidate.mask.entry_count() == manifest_len
        && Arc::ptr_eq(&candidate.context.entry_indices, &metadata.entry_indices)
        && metadata.root_identity == *root_identity
        && metadata.directory_generation == directory_generation
}

/// Rebase retained ready-mask provenance after the audited metadata revision advances.
pub(super) fn rebase_ready_context(
    browser: &mut Browser,
    revision: u64,
    root_identity: &RootIdentity,
    directory_generation: u64,
) -> bool {
    let Some(context) = current_context(browser) else {
        return false;
    };
    if context.revision != revision
        || context.root_identity != *root_identity
        || context.directory_generation != directory_generation
        || browser.tag_filter.ready_draft_generation != Some(browser.tag_filter.draft_generation)
        || browser.view_model.tag_filter().mask().is_none()
    {
        return false;
    }
    browser.tag_filter.ready_context = Some(context);
    true
}

/// Reapply a previously validated positional mask after a synchronous metadata replacement.
pub(super) fn apply_reusable_mask(browser: &mut Browser, mask: TagFilterMask) -> bool {
    apply_reusable_mask_to_model(&mut browser.view_model, &browser.entries, mask)
}

fn apply_reusable_mask_to_model(
    view_model: &mut BrowserViewModel,
    entries: &[AudioFile],
    mask: TagFilterMask,
) -> bool {
    view_model.apply_tag_filter(entries, mask).is_ok()
}

/// Return whether the source poll lane needs another tag-filter attempt.
pub(super) fn has_pending(browser: &Browser) -> bool {
    if browser.tag_filter.pending.is_some() {
        return true;
    }
    browser.tag_filter.effective_enabled()
        && browser.tag_filter.retry_ready
        && current_context(browser).is_some()
}

/// Return the bounded status text for the filter control and browser list.
pub(super) fn status_label(browser: &Browser) -> Option<String> {
    browser
        .tag_filter
        .input_error
        .as_ref()
        .or(browser.tag_filter.status.as_ref())
        .cloned()
}

/// Submit one bounded tag-filter read or consume a bounded admission failure.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
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
        if browser.tag_filter.pending.is_some() || browser.tag_filter.retry_ready {
            browser.tag_filter.clear_request();
            browser.tag_filter.status = Some(String::from(
                "Tag filtering is waiting for the source catalog",
            ));
            return true;
        }
        return false;
    }
    if !browser.tag_filter.effective_enabled()
        || browser.tag_filter.pending.is_some()
        || !browser.tag_filter.retry_ready
        || browser.curation_operation_pending()
    {
        return false;
    }
    let Some(context) = current_context(browser) else {
        return false;
    };
    let request_id = browser.next_tag_request_id();
    let pending = PendingFilter {
        request_id,
        draft_generation: browser.tag_filter.draft_generation,
        context: context.clone(),
    };
    let command = CatalogCommand::RequestTagFilter {
        source_id: context.source_id.clone(),
        lifecycle_generation: context.lifecycle_generation,
        expected_revision: context.revision,
        request_id,
        expected_manifest_len: context.manifest_len,
        required_tags: browser.tag_filter.requirements.clone(),
    };
    browser.tag_filter.pending = Some(pending);
    browser.tag_filter.retry_ready = false;
    match catalog::submit_catalog_command(command, cx) {
        Ok(()) => true,
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
            browser.tag_filter.pending = None;
            browser.tag_filter.retry_ready = true;
            false
        }
        Err(error) => {
            browser.tag_filter.pending = None;
            browser.tag_filter.retry_ready = false;
            browser.tag_filter.status = Some(format!("Tag filter unavailable: {error}"));
            true
        }
    }
}

/// Consume a source tag-filter event after checking every projection fence.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    match event {
        CatalogEvent::TagFilterReady {
            source_id,
            lifecycle_generation,
            root_identity,
            directory_generation,
            revision,
            manifest_len,
            request_id,
            mask,
        } => apply_ready(
            browser,
            &source_id,
            lifecycle_generation,
            &root_identity,
            directory_generation,
            revision,
            manifest_len,
            request_id,
            mask,
            cx,
        ),
        CatalogEvent::TagFilterFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            observed_revision,
            request_id,
            error,
        } => apply_failed(
            browser,
            &source_id,
            lifecycle_generation,
            expected_revision,
            observed_revision,
            request_id,
            error,
            cx,
        ),
        _ => false,
    }
}

fn apply_ready(
    browser: &mut Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
    root_identity: &RootIdentity,
    directory_generation: u64,
    revision: u64,
    manifest_len: usize,
    request_id: u64,
    words: Vec<u64>,
    cx: &mut Context<Browser>,
) -> bool {
    let Some(pending) = browser.tag_filter.pending.clone() else {
        return false;
    };
    if !pending_request_matches(
        &pending,
        source_id,
        lifecycle_generation,
        root_identity,
        directory_generation,
        revision,
        manifest_len,
        request_id,
    ) {
        return false;
    }
    if browser
        .tag_filter
        .consume_superseded_response(pending.draft_generation)
    {
        return true;
    }
    let Some(current) = current_context(browser) else {
        browser.tag_filter.pending = None;
        browser.tag_filter.retry_ready = false;
        browser.tag_filter.status = Some(String::from(
            "Tag filter result is waiting for a current catalog projection",
        ));
        return true;
    };
    if !contexts_match(&pending.context, &current) {
        browser.tag_filter.pending = None;
        browser.tag_filter.retry_ready = browser.tag_filter.effective_enabled();
        return true;
    }
    let Ok(mask) = TagFilterMask::try_from_words(manifest_len, Arc::from(words)) else {
        browser.tag_filter.pending = None;
        browser.tag_filter.retry_ready = false;
        browser.tag_filter.status =
            Some(String::from("Tag filter returned an invalid source mask"));
        return true;
    };
    let origin = browser.tag_filter.focus_origin;
    let fallback_focus = current_focus(browser);
    let focus = browser
        .tag_filter
        .focus_intent
        .take()
        .unwrap_or(fallback_focus);
    browser.tag_filter.pending = None;
    browser.tag_filter.retry_ready = false;
    browser.tag_filter.status = None;
    if browser
        .view_model
        .apply_tag_filter(&browser.entries, mask)
        .is_err()
    {
        browser.tag_filter.retry_ready = browser.tag_filter.effective_enabled();
        browser.tag_filter.status = Some(String::from(
            "Tag filter result no longer matches the browser projection",
        ));
        return true;
    }
    browser.tag_filter.ready_context = Some(current);
    browser.tag_filter.ready_draft_generation = Some(browser.tag_filter.draft_generation);
    browser.refresh_collection_filter_counts();
    browser.reconcile_selection();
    match origin {
        FilterOrigin::Explicit => {
            browser.reset_browser_focus_state();
            browser.restore_projection_focus(focus.id, focus.position, cx);
        }
        FilterOrigin::PassiveMetadata | FilterOrigin::PassivePlayback => {
            browser.restore_playback_type_projection_focus(focus.id, focus.position, cx);
        }
    }
    true
}

fn apply_failed(
    browser: &mut Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    observed_revision: Option<u64>,
    request_id: u64,
    error: wavecrate_session::CatalogError,
    _cx: &mut Context<Browser>,
) -> bool {
    let Some(pending) = browser.tag_filter.pending.clone() else {
        return false;
    };
    if !pending_failed_request_matches(
        &pending,
        source_id,
        lifecycle_generation,
        expected_revision,
        request_id,
    ) {
        return false;
    }
    if browser
        .tag_filter
        .consume_superseded_response(pending.draft_generation)
    {
        return true;
    }
    let Some(current) = current_context(browser) else {
        browser.tag_filter.pending = None;
        browser.tag_filter.retry_ready = false;
        browser.tag_filter.status = Some(String::from(
            "Tag filter failure is waiting for a current catalog projection",
        ));
        return true;
    };
    if !contexts_match(&pending.context, &current) {
        browser.tag_filter.pending = None;
        browser.tag_filter.retry_ready = browser.tag_filter.effective_enabled();
        return true;
    }
    browser.tag_filter.pending = None;
    browser.tag_filter.retry_ready = false;
    browser.tag_filter.status = Some(
        if observed_revision.is_some_and(|revision| revision != expected_revision) {
            format!("Tag filter is waiting for catalog revision: {error}")
        } else {
            format!("Tag filter failed: {error}")
        },
    );
    true
}

fn pending_request_matches(
    pending: &PendingFilter,
    source_id: &SourceId,
    lifecycle_generation: u64,
    root_identity: &RootIdentity,
    directory_generation: u64,
    revision: u64,
    manifest_len: usize,
    request_id: u64,
) -> bool {
    pending.request_id == request_id
        && pending.context.source_id == *source_id
        && pending.context.lifecycle_generation == lifecycle_generation
        && pending.context.root_identity == *root_identity
        && pending.context.directory_generation == directory_generation
        && pending.context.revision == revision
        && pending.context.manifest_len == manifest_len
}

fn pending_failed_request_matches(
    pending: &PendingFilter,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
) -> bool {
    pending.request_id == request_id
        && pending.context.source_id == *source_id
        && pending.context.lifecycle_generation == lifecycle_generation
        && pending.context.revision == expected_revision
}

fn current_context(browser: &Browser) -> Option<FilterContext> {
    let activation = browser.catalog_activation.as_ref()?;
    if !activation.audit_finished || activation.audit_error.is_some() {
        return None;
    }
    let metadata = browser.catalog_metadata.context()?;
    let key = curation::CurationLifecycleKey {
        source_id: activation.source_id.clone(),
        lifecycle_generation: activation.lifecycle_generation,
    };
    if metadata.source_id != key.source_id
        || metadata.lifecycle_generation != key.lifecycle_generation
        || metadata.root != activation.root
        || metadata.entry_indices.len() != browser.entries.len()
        || !browser.curation_state.authority_matches(
            &key,
            &metadata.root_identity,
            metadata.directory_generation,
            metadata.revision,
        )
    {
        return None;
    }
    Some(FilterContext {
        source_id: key.source_id,
        lifecycle_generation: key.lifecycle_generation,
        root: metadata.root,
        root_identity: metadata.root_identity,
        directory_generation: metadata.directory_generation,
        revision: metadata.revision,
        manifest_len: browser.entries.len(),
        entry_indices: metadata.entry_indices,
    })
}

fn contexts_match(left: &FilterContext, right: &FilterContext) -> bool {
    left.source_id == right.source_id
        && left.lifecycle_generation == right.lifecycle_generation
        && left.root == right.root
        && left.root_identity == right.root_identity
        && left.directory_generation == right.directory_generation
        && left.revision == right.revision
        && left.manifest_len == right.manifest_len
        && Arc::ptr_eq(&left.entry_indices, &right.entry_indices)
}

/// Render the compact runtime tag-filter control.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let enabled = browser.tag_filter.desired_enabled;
    let status = status_label(browser);
    let toggle_text_color = if enabled {
        color(theme.colors.accent)
    } else {
        color(theme.colors.text_muted)
    };
    let toggle_hover = color(theme.colors.selection_hover_fill);
    let toggle_pressed = color(theme.interaction.pressed_fill);
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
                        .id("tag-filter-toggle")
                        .flex_none()
                        .h(px(theme.spacing.row_height))
                        .items_center()
                        .cursor_pointer()
                        .text_color(toggle_text_color)
                        .hover(move |style| style.bg(toggle_hover))
                        .active(move |style| style.bg(toggle_pressed))
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            toggle(view, cx);
                        }))
                        .child("TAGS"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(browser.tag_filter_input.clone()),
                ),
        )
        .children(status.into_iter().map(|message| {
            div()
                .px(px(theme.spacing.panel_padding))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.warning))
                .child(message)
        }))
}

#[cfg(test)]
mod tests {
    use super::{
        DraftAction, FilterContext, FilterOrigin, PendingFilter, ReusableMaskCandidate,
        TagFilterState, apply_reusable_mask_to_model, contexts_match, reusable_candidate_matches,
        should_replace_focus,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use wavecrate_core::{
        AudioFile, AudioFileCuration, BrowserViewModel, SortColumn, TagFilterMask,
    };
    use wavecrate_library::sample_sources::{SourceId, reconciliation::RootIdentity};

    fn context(indices: Arc<HashMap<PathBuf, usize>>) -> FilterContext {
        FilterContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/library"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 4,
            revision: 7,
            manifest_len: indices.len(),
            entry_indices: indices,
        }
    }

    fn audio_file(path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(path.rsplit_once('/').map_or(path, |(_, path)| path)),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn empty_requirements_have_no_effective_filter() {
        let mut state = TagFilterState::default();
        state.desired_enabled = true;
        assert!(!state.effective_enabled());
        state.requirements.push(String::from("kick"));
        assert!(state.effective_enabled());
    }

    #[test]
    fn draft_edits_toggle_off_and_on_coalesce_behind_one_pending_request() {
        let indices = Arc::new(HashMap::from([(PathBuf::from("a.wav"), 0)]));
        let mut state = TagFilterState {
            desired_enabled: true,
            requirements: vec![String::from("old")],
            draft_generation: 4,
            pending: Some(PendingFilter {
                request_id: 9,
                draft_generation: 4,
                context: context(Arc::clone(&indices)),
            }),
            ..Default::default()
        };
        assert_eq!(
            state.apply_valid_draft(vec![String::from("new")]),
            DraftAction::Refresh
        );
        assert_eq!(state.toggle_desired(), DraftAction::Disable);
        assert_eq!(state.toggle_desired(), DraftAction::Refresh);
        assert!(state.pending.is_some());
        assert!(state.consume_superseded_response(4));
        assert!(state.pending.is_none());
        assert!(state.retry_ready);
        assert!(!state.consume_superseded_response(4));
    }

    #[test]
    fn invalid_draft_remains_blocked_across_toggle_off_and_on() {
        let mut state = TagFilterState {
            desired_enabled: true,
            requirements: vec![String::from("previous")],
            ..Default::default()
        };
        assert_eq!(
            state.apply_invalid_draft(String::from("empty tag")),
            DraftAction::HoldInvalid
        );
        assert!(!state.effective_enabled());
        assert!(state.filter_waiting());
        assert_eq!(state.toggle_desired(), DraftAction::Disable);
        assert_eq!(state.toggle_desired(), DraftAction::HoldInvalid);
        assert!(state.input_error.is_some());
        assert!(!state.effective_enabled());
    }

    #[test]
    fn explicit_focus_survives_passive_refresh_but_overrides_passive_focus() {
        let intent = super::FocusIntent {
            id: None,
            position: Some(4),
        };
        assert!(!should_replace_focus(
            Some(&intent),
            FilterOrigin::Explicit,
            FilterOrigin::PassiveMetadata
        ));
        assert!(!should_replace_focus(
            Some(&intent),
            FilterOrigin::PassiveMetadata,
            FilterOrigin::PassivePlayback
        ));
        assert!(should_replace_focus(
            Some(&intent),
            FilterOrigin::PassiveMetadata,
            FilterOrigin::Explicit
        ));
        assert!(should_replace_focus(
            None,
            FilterOrigin::PassiveMetadata,
            FilterOrigin::PassiveMetadata
        ));
    }

    #[test]
    fn context_matching_requires_entry_index_arc_identity() {
        let first = Arc::new(HashMap::from([(PathBuf::from("a.wav"), 0)]));
        let same_contents = Arc::new(HashMap::from([(PathBuf::from("a.wav"), 0)]));
        assert!(contexts_match(
            &context(Arc::clone(&first)),
            &context(Arc::clone(&first))
        ));
        assert!(!contexts_match(&context(first), &context(same_contents)));
    }

    #[test]
    fn reusable_mask_requires_complete_non_tag_receipt_provenance() {
        let indices = Arc::new(HashMap::from([
            (PathBuf::from("a.wav"), 0),
            (PathBuf::from("b.wav"), 1),
        ]));
        let metadata = context(Arc::clone(&indices));
        let root_identity = metadata.root_identity.clone();
        let candidate = ReusableMaskCandidate {
            context: metadata.clone(),
            draft_generation: 8,
            mask: TagFilterMask::try_from_vec(2, vec![1]).expect("valid test mask"),
        };
        let valid = || {
            reusable_candidate_matches(
                &candidate,
                &metadata,
                8,
                7,
                8,
                &root_identity,
                4,
                2,
                false,
                true,
            )
        };
        assert!(valid());
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            7,
            8,
            &root_identity,
            4,
            2,
            true,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            6,
            8,
            &root_identity,
            4,
            2,
            false,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            7,
            8,
            &RootIdentity::from_bytes(b"other-root".to_vec()),
            4,
            2,
            false,
            true,
        ));
        let different_indices = Arc::new(HashMap::from([
            (PathBuf::from("a.wav"), 0),
            (PathBuf::from("b.wav"), 1),
        ]));
        let different_metadata = context(different_indices);
        assert!(!reusable_candidate_matches(
            &candidate,
            &different_metadata,
            8,
            7,
            8,
            &root_identity,
            4,
            2,
            false,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            9,
            7,
            8,
            &root_identity,
            4,
            2,
            false,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            7,
            8,
            &root_identity,
            3,
            2,
            false,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            7,
            8,
            &root_identity,
            4,
            1,
            false,
            true,
        ));
        let different_source = FilterContext {
            source_id: SourceId::from_string("other-source"),
            ..metadata.clone()
        };
        assert!(!reusable_candidate_matches(
            &candidate,
            &different_source,
            8,
            7,
            8,
            &root_identity,
            4,
            2,
            false,
            true,
        ));
        assert!(!reusable_candidate_matches(
            &candidate,
            &metadata,
            8,
            7,
            8,
            &root_identity,
            4,
            2,
            false,
            false,
        ));
    }

    #[test]
    fn reusable_mask_restores_matching_rows_after_entry_replacement() {
        let mut match_entry = audio_file("/library/match.wav");
        match_entry.curation = AudioFileCuration::from_parts(0, false, 0, None, None);
        let mut other_entry = audio_file("/library/other.wav");
        other_entry.curation = AudioFileCuration::from_parts(1, false, 0, None, None);
        let entries = vec![match_entry, other_entry];
        let mut model = BrowserViewModel::new(&entries);
        model.toggle_sort(&entries, SortColumn::Rating);
        assert!(model.begin_tag_filter(&entries));
        let mask = TagFilterMask::try_from_vec(entries.len(), vec![1]).expect("valid test mask");
        assert!(apply_reusable_mask_to_model(
            &mut model,
            &entries,
            mask.clone()
        ));
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].id, entries[0].stable_id());

        let mut replacement_match = entries[0].clone();
        replacement_match.curation = AudioFileCuration::from_parts(2, false, 0, None, None);
        let mut replacement_other = entries[1].clone();
        replacement_other.curation = AudioFileCuration::from_parts(-1, false, 0, None, None);
        let replacement = vec![replacement_match, replacement_other];
        let mut sorted_replacement = BrowserViewModel::new(&replacement);
        sorted_replacement.toggle_sort(&replacement, SortColumn::Rating);
        assert_eq!(sorted_replacement.rows()[0].id, replacement[1].stable_id());
        model.set_entries(&replacement);
        assert!(model.tag_filter().is_pending());
        assert!(apply_reusable_mask_to_model(&mut model, &replacement, mask));
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].id, replacement[0].stable_id());
    }

    #[test]
    fn core_pending_state_is_distinct_from_disabled() {
        assert!(!wavecrate_core::TagFilterState::disabled().is_pending());
    }
}
