//! Deferred, revision-fenced display-label projections for the browser name column.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::Context;
use wavecrate_core::{SampleNameViewMode, sample_display_name};
use wavecrate_library::sample_sources::{
    SourceDisplayLabelEntry, SourceId, db::normalize_relative_path, reconciliation::RootIdentity,
};
use wavecrate_session::{
    CatalogCommand, CatalogEvent, CatalogSubmitError, SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATHS,
};

use super::{Browser, catalog, catalog_metadata};

/// The maximum number of source-relative labels retained by the browser.
pub(super) const DISPLAY_LABEL_CACHE_MAX_ENTRIES: usize = 256;
/// The maximum aggregate UTF-8 bytes retained for cached labels.
pub(super) const DISPLAY_LABEL_CACHE_MAX_LABEL_BYTES: usize = 1024 * 1024;
/// The maximum aggregate bytes retained for cached source-relative path keys.
pub(super) const DISPLAY_LABEL_CACHE_MAX_PATH_BYTES: usize = 1024 * 1024;
/// The maximum number of visible paths kept in the latest demand window.
pub(super) const MAX_VISIBLE_DEMAND_PATHS: usize = DISPLAY_LABEL_CACHE_MAX_ENTRIES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DisplayLabelsContext {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
    pub(super) root: PathBuf,
    pub(super) root_identity: RootIdentity,
    pub(super) directory_generation: u64,
    pub(super) revision: u64,
    pub(super) entry_indices: Arc<HashMap<PathBuf, usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DisplayLabelsRequest {
    context: DisplayLabelsContext,
    request_id: u64,
    paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CachedDisplayLabel {
    label: Option<String>,
    truncated: bool,
}

/// Bounded browser state for one deferred display-label lane.
pub(super) struct DisplayLabelsState {
    context: Option<DisplayLabelsContext>,
    latest_demand: Vec<PathBuf>,
    terminal_paths: BTreeSet<PathBuf>,
    cache: HashMap<PathBuf, CachedDisplayLabel>,
    cache_label_bytes: usize,
    cache_path_bytes: usize,
    pending: Option<DisplayLabelsRequest>,
    retry_ready: bool,
    paused: bool,
    next_request_id: u64,
    error: Option<String>,
}

impl Default for DisplayLabelsState {
    fn default() -> Self {
        Self {
            context: None,
            latest_demand: Vec::new(),
            terminal_paths: BTreeSet::new(),
            cache: HashMap::new(),
            cache_label_bytes: 0,
            cache_path_bytes: 0,
            pending: None,
            retry_ready: false,
            paused: false,
            next_request_id: 0,
            error: None,
        }
    }
}

impl DisplayLabelsState {
    pub(super) fn reset(&mut self) {
        self.context = None;
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.clear_cache();
        self.pending = None;
        self.retry_ready = false;
        self.paused = false;
        self.error = None;
    }

    pub(super) fn cancel_for_source_change(&mut self) {
        self.reset();
    }

    pub(super) fn reset_for_projection(&mut self) {
        self.context = None;
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.clear_cache();
        self.retry_ready = false;
        self.paused = false;
        self.error = None;
    }

    pub(super) fn install_projection(&mut self, context: DisplayLabelsContext) {
        let pending = self.pending.take();
        self.reset_for_projection();
        self.context = Some(context);
        self.pending = pending.filter(|request| {
            self.context
                .as_ref()
                .is_some_and(|current| same_lifecycle(&request.context, current))
        });
    }

    fn replace_projection_preserving_pending(&mut self, context: DisplayLabelsContext) {
        let pending = self
            .pending
            .take()
            .filter(|request| same_lifecycle(&request.context, &context));
        self.context = Some(context);
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.clear_cache();
        self.retry_ready = false;
        self.paused = false;
        self.error = None;
        self.pending = pending;
    }

    pub(super) fn sync_catalog_projection(
        &mut self,
        context: Option<DisplayLabelsContext>,
        curation_pending: bool,
    ) {
        if curation_pending {
            self.pause_new_reads();
            return;
        }
        let Some(context) = context else {
            self.reset_for_projection();
            return;
        };
        if !contexts_match(self.context.as_ref(), Some(&context)) {
            self.replace_projection_preserving_pending(context);
        } else if self.paused {
            self.paused = false;
            self.error = None;
        }
    }

    /// Clear projected labels while a curation receipt may change source tags.
    pub(super) fn pause_for_curation(&mut self, tags_may_change: bool) {
        self.pause_new_reads();
        if tags_may_change {
            self.latest_demand.clear();
            self.terminal_paths.clear();
            self.clear_cache();
        }
    }

    fn pause_new_reads(&mut self) {
        self.retry_ready = false;
        self.paused = true;
        self.error = None;
    }

    /// Rebase a confirmed non-tag receipt while retaining the current label working set.
    pub(super) fn rebase_after_curation(
        &mut self,
        context: DisplayLabelsContext,
        prior_revision: u64,
        final_revision: u64,
        root_identity: &RootIdentity,
        directory_generation: u64,
        curation_pending: bool,
    ) -> bool {
        let Some(previous) = self.context.as_ref() else {
            return false;
        };
        if final_revision < prior_revision
            || context.revision != final_revision
            || previous.revision != prior_revision
            || previous.source_id != context.source_id
            || previous.lifecycle_generation != context.lifecycle_generation
            || previous.root != context.root
            || previous.root_identity != *root_identity
            || previous.root_identity != context.root_identity
            || previous.directory_generation != directory_generation
            || previous.directory_generation != context.directory_generation
            || !Arc::ptr_eq(&previous.entry_indices, &context.entry_indices)
        {
            return false;
        }
        self.context = Some(context);
        self.paused = curation_pending;
        self.error = None;
        self.retry_ready =
            !curation_pending && self.pending.is_none() && !self.next_paths().is_empty();
        true
    }

    /// Drop labels after an uncertain or tag-changing receipt while retaining an admitted reply
    /// long enough to drain it under its original request fence.
    pub(super) fn invalidate_after_curation(
        &mut self,
        context: Option<DisplayLabelsContext>,
        curation_pending: bool,
    ) {
        self.context = context;
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.clear_cache();
        self.retry_ready = false;
        self.paused = curation_pending;
        self.error = None;
    }

    /// Requeue the current working set after an admitted reply from an older revision drains.
    pub(super) fn retry_after_drained_reply(&mut self) {
        if !self.paused && self.error.is_none() && self.pending.is_none() {
            self.retry_ready = !self.next_paths().is_empty();
        }
    }

    pub(super) fn disable_mode(&mut self) {
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.retry_ready = false;
        self.error = None;
    }

    pub(super) fn context(&self) -> Option<DisplayLabelsContext> {
        self.context.clone()
    }

    pub(super) fn observe_visible_demand(
        &mut self,
        context: DisplayLabelsContext,
        paths: Vec<PathBuf>,
    ) -> bool {
        if self.paused || !contexts_match(self.context.as_ref(), Some(&context)) {
            return false;
        }

        let mut latest = Vec::with_capacity(paths.len().min(MAX_VISIBLE_DEMAND_PATHS));
        let mut path_bytes = 0usize;
        for path in paths {
            if latest.len() >= MAX_VISIBLE_DEMAND_PATHS
                || !context.entry_indices.contains_key(&path)
                || latest.iter().any(|existing| existing == &path)
            {
                continue;
            }
            let bytes = path_byte_len(&path);
            let Some(next_bytes) = path_bytes.checked_add(bytes) else {
                break;
            };
            if next_bytes > DISPLAY_LABEL_CACHE_MAX_PATH_BYTES {
                continue;
            }
            path_bytes = next_bytes;
            latest.push(path);
        }
        self.latest_demand = latest;
        self.retain_working_set();
        if self.next_paths().is_empty() {
            self.retry_ready = false;
            return false;
        }
        if self.pending.is_some() {
            return false;
        }
        self.error = None;
        self.retry_ready = true;
        true
    }

    fn begin_request(&mut self) -> Option<DisplayLabelsRequest> {
        if self.paused || self.pending.is_some() || !self.retry_ready || self.error.is_some() {
            return None;
        }
        let context = self.context.clone()?;
        let mut paths = Vec::with_capacity(SOURCE_DISPLAY_LABEL_MAX_PATHS);
        let mut path_bytes = 0usize;
        for path in self.next_paths() {
            let Some(bytes) = request_path_byte_len(&path) else {
                self.mark_terminal(std::slice::from_ref(&path));
                continue;
            };
            if bytes > SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES {
                self.mark_terminal(std::slice::from_ref(&path));
                continue;
            }
            if paths.len() >= SOURCE_DISPLAY_LABEL_MAX_PATHS
                || path_bytes
                    .checked_add(bytes)
                    .is_none_or(|total| total > SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES)
            {
                break;
            }
            path_bytes += bytes;
            paths.push(path);
        }
        if paths.is_empty() {
            self.retry_ready = !self.next_paths().is_empty();
            return None;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        let request = DisplayLabelsRequest {
            context,
            request_id: self.next_request_id,
            paths,
        };
        self.retry_ready = false;
        self.pending = Some(request.clone());
        Some(request)
    }

    fn admission_failed(
        &mut self,
        request_id: u64,
        retryable: bool,
        error: Option<String>,
    ) -> bool {
        let Some(request) = self.pending.take() else {
            return false;
        };
        if request.request_id != request_id {
            self.pending = Some(request);
            return false;
        }
        if retryable {
            self.retry_ready = true;
        } else {
            self.mark_terminal(&request.paths);
            self.retry_ready = false;
        }
        self.error = error;
        true
    }

    fn ready_request(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        root_identity: &RootIdentity,
        directory_generation: u64,
        revision: u64,
        request_id: u64,
        requested_paths: &[PathBuf],
    ) -> Option<DisplayLabelsRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id
            && request.context.source_id == *source_id
            && request.context.lifecycle_generation == lifecycle_generation
            && request.context.root_identity == *root_identity
            && request.context.directory_generation == directory_generation
            && request.context.revision == revision
            && request.paths == requested_paths)
            .then(|| request.clone())
    }

    fn failed_request(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
    ) -> Option<DisplayLabelsRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id
            && request.context.source_id == *source_id
            && request.context.lifecycle_generation == lifecycle_generation
            && request.context.revision == revision)
            .then(|| request.clone())
    }

    fn take_pending(&mut self, request_id: u64) -> Option<DisplayLabelsRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id)
            .then(|| self.pending.take().expect("request was checked"))
    }

    fn apply_ready(
        &mut self,
        request: &DisplayLabelsRequest,
        requested_paths: &[PathBuf],
        entries: &[SourceDisplayLabelEntry],
    ) -> Result<(), String> {
        validate_ready_payload(request, requested_paths, entries)?;
        self.mark_terminal(requested_paths);
        for entry in entries {
            if !self.latest_demand.contains(&entry.relative_path) {
                continue;
            }
            if entry.display_label.as_ref().is_some_and(String::is_empty) {
                continue;
            }
            let cached = self.insert_cache(
                entry.relative_path.clone(),
                CachedDisplayLabel {
                    label: entry.display_label.clone(),
                    truncated: entry.truncated,
                },
            );
            if !cached {
                self.mark_terminal(std::slice::from_ref(&entry.relative_path));
            }
        }
        self.error = None;
        self.retry_ready = !self.next_paths().is_empty();
        Ok(())
    }

    fn fail_request(&mut self, request: &DisplayLabelsRequest, error: String) {
        self.mark_terminal(&request.paths);
        self.retry_ready = false;
        self.error = Some(error);
    }

    fn next_paths(&self) -> Vec<PathBuf> {
        let pending_paths = self
            .pending
            .as_ref()
            .map(|request| request.paths.as_slice())
            .unwrap_or_default();
        self.latest_demand
            .iter()
            .filter(|path| !self.terminal_paths.contains(*path) && !pending_paths.contains(path))
            .cloned()
            .collect()
    }

    pub(super) fn poll_needed(&self) -> bool {
        self.pending.is_some() || self.retry_ready
    }

    pub(super) fn status_label(&self) -> Option<String> {
        self.error.as_ref().map(|error| format!("Labels: {error}"))
    }

    pub(super) fn display_name(
        &self,
        relative_path: &Path,
        mode: SampleNameViewMode,
    ) -> (String, bool) {
        let cached = self.cache.get(relative_path);
        let label = cached.and_then(|value| value.label.as_deref());
        let mut display = sample_display_name(relative_path, label, mode).into_owned();
        let truncated = mode == SampleNameViewMode::MetadataLabel
            && cached.is_some_and(|value| value.truncated && value.label.is_some());
        if truncated {
            display.push('…');
        }
        (display, truncated)
    }

    fn mark_terminal(&mut self, paths: &[PathBuf]) {
        self.terminal_paths.extend(
            paths
                .iter()
                .filter(|path| self.latest_demand.contains(path))
                .cloned(),
        );
    }

    fn retain_working_set(&mut self) {
        self.cache
            .retain(|path, _| self.latest_demand.contains(path));
        self.terminal_paths
            .retain(|path| self.latest_demand.contains(path));
        self.cache_label_bytes = self
            .cache
            .values()
            .map(|value| value.label.as_ref().map_or(0, String::len))
            .sum();
        self.cache_path_bytes = self.cache.keys().map(|path| path_byte_len(path)).sum();
    }

    fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache_label_bytes = 0;
        self.cache_path_bytes = 0;
    }

    fn insert_cache(&mut self, path: PathBuf, value: CachedDisplayLabel) -> bool {
        if self.cache.contains_key(&path) {
            return true;
        }
        let path_bytes = path_byte_len(&path);
        let label_bytes = value.label.as_ref().map_or(0, String::len);
        if self.cache.len() >= DISPLAY_LABEL_CACHE_MAX_ENTRIES
            || self
                .cache_path_bytes
                .checked_add(path_bytes)
                .is_none_or(|bytes| bytes > DISPLAY_LABEL_CACHE_MAX_PATH_BYTES)
            || self
                .cache_label_bytes
                .checked_add(label_bytes)
                .is_none_or(|bytes| bytes > DISPLAY_LABEL_CACHE_MAX_LABEL_BYTES)
        {
            return false;
        }
        self.cache_path_bytes += path_bytes;
        self.cache_label_bytes += label_bytes;
        self.cache.insert(path, value);
        true
    }
}

fn validate_ready_payload(
    request: &DisplayLabelsRequest,
    requested_paths: &[PathBuf],
    entries: &[SourceDisplayLabelEntry],
) -> Result<(), String> {
    if requested_paths.len() != request.paths.len() || requested_paths != request.paths {
        return Err(String::from(
            "display-label reply paths do not match its request",
        ));
    }
    if requested_paths.len() > SOURCE_DISPLAY_LABEL_MAX_PATHS {
        return Err(String::from("display-label reply contains too many paths"));
    }
    let mut path_bytes = 0usize;
    let mut requested = BTreeSet::new();
    for path in requested_paths {
        if !request.context.entry_indices.contains_key(path) {
            return Err(String::from("display-label reply named an unknown path"));
        }
        if !requested.insert(path) {
            return Err(String::from(
                "display-label reply repeated a requested path",
            ));
        }
        path_bytes = path_bytes
            .checked_add(
                request_path_byte_len(path)
                    .ok_or_else(|| String::from("display-label reply path is invalid"))?,
            )
            .ok_or_else(|| String::from("display-label reply path bytes overflowed"))?;
    }
    if path_bytes > SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES {
        return Err(String::from(
            "display-label reply paths exceeded their byte bound",
        ));
    }
    if entries.len() > requested_paths.len() {
        return Err(String::from(
            "display-label reply contains too many entries",
        ));
    }
    let mut returned = BTreeSet::new();
    let mut label_bytes = 0usize;
    for entry in entries {
        if !requested.contains(&entry.relative_path) || !returned.insert(&entry.relative_path) {
            return Err(String::from(
                "display-label reply contains an unknown or repeated entry",
            ));
        }
        if entry
            .display_label
            .as_ref()
            .is_some_and(|label| label.len() > SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES)
        {
            return Err(String::from(
                "display-label reply label exceeded its byte bound",
            ));
        }
        label_bytes = label_bytes
            .checked_add(entry.display_label.as_ref().map_or(0, String::len))
            .ok_or_else(|| String::from("display-label reply label bytes overflowed"))?;
    }
    if label_bytes > DISPLAY_LABEL_CACHE_MAX_LABEL_BYTES {
        return Err(String::from(
            "display-label reply labels exceeded their cache bound",
        ));
    }
    Ok(())
}

fn path_byte_len(path: &Path) -> usize {
    encoded_path_len(path)
}

fn request_path_byte_len(path: &Path) -> Option<usize> {
    encoded_path_len(path).checked_add(normalize_relative_path(path).ok()?.len())
}

#[cfg(unix)]
fn encoded_path_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn encoded_path_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

impl Browser {
    pub(super) fn toggle_sample_name_view_mode(&mut self, cx: &mut Context<Self>) {
        self.sample_name_view_mode = self.sample_name_view_mode.toggled();
        if self.sample_name_view_mode == SampleNameViewMode::MetadataLabel {
            self.start_source_registry_poll(cx);
        } else {
            self.display_labels.disable_mode();
        }
        cx.notify();
    }

    pub(super) fn sample_name_display(&self, relative_path: &Path) -> (String, bool) {
        self.display_labels
            .display_name(relative_path, self.sample_name_view_mode)
    }

    pub(super) fn observe_display_labels(
        &mut self,
        context: DisplayLabelsContext,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.sample_name_view_mode != SampleNameViewMode::MetadataLabel {
            return;
        }
        if self.display_labels.observe_visible_demand(context, paths) {
            self.start_source_registry_poll(cx);
        }
    }

    pub(super) fn poll_display_labels(&mut self, cx: &mut Context<Self>) -> bool {
        self.sync_display_labels_projection();
        if self.sample_name_view_mode != SampleNameViewMode::MetadataLabel {
            return false;
        }
        let Some(request) = self.display_labels.begin_request() else {
            return false;
        };
        let request_id = request.request_id;
        match catalog::submit_catalog_command(request.into_command(), cx) {
            Ok(()) => true,
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {
                self.display_labels.admission_failed(request_id, true, None)
            }
            Err(error) => self.display_labels.admission_failed(
                request_id,
                false,
                Some(format!("could not load visible display labels: {error}")),
            ),
        }
    }

    pub(super) fn has_pending_display_labels(&self) -> bool {
        self.display_labels.pending.is_some()
            || (self.sample_name_view_mode == SampleNameViewMode::MetadataLabel
                && self.display_labels.poll_needed())
    }

    pub(super) fn sync_display_labels_projection(&mut self) {
        let context = self
            .catalog_metadata
            .context()
            .map(display_context_from_metadata);
        let curation_pending = context
            .as_ref()
            .is_some_and(|context| curation_pending_for_context(&self.curation_state, context));
        self.display_labels
            .sync_catalog_projection(context, curation_pending);
    }

    pub(super) fn apply_display_labels_event(&mut self, event: &CatalogEvent) -> bool {
        match event {
            CatalogEvent::DisplayLabelsReady {
                source_id,
                lifecycle_generation,
                root_identity,
                directory_generation,
                revision,
                request_id,
                requested_paths,
                entries,
            } => {
                let Some(request) = self.display_labels.ready_request(
                    source_id,
                    *lifecycle_generation,
                    root_identity,
                    *directory_generation,
                    *revision,
                    *request_id,
                    requested_paths,
                ) else {
                    return false;
                };
                let current =
                    contexts_match(self.display_labels.context.as_ref(), Some(&request.context));
                let active = context_is_active(&request.context, self);
                let Some(request) = self.display_labels.take_pending(*request_id) else {
                    return false;
                };
                if self.sample_name_view_mode != SampleNameViewMode::MetadataLabel
                    || self.display_labels.paused
                    || !current
                    || !active
                {
                    if active {
                        self.display_labels.retry_after_drained_reply();
                    }
                    return true;
                }
                if let Err(error) =
                    self.display_labels
                        .apply_ready(&request, requested_paths, entries)
                {
                    self.display_labels
                        .fail_request(&request, format!("invalid display-label reply: {error}"));
                }
                true
            }
            CatalogEvent::DisplayLabelsFailed {
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                error,
                ..
            } => {
                let Some(request) = self.display_labels.failed_request(
                    source_id,
                    *lifecycle_generation,
                    *expected_revision,
                    *request_id,
                ) else {
                    return false;
                };
                let current =
                    contexts_match(self.display_labels.context.as_ref(), Some(&request.context));
                let active = context_is_active(&request.context, self);
                let Some(request) = self.display_labels.take_pending(*request_id) else {
                    return false;
                };
                if self.sample_name_view_mode != SampleNameViewMode::MetadataLabel
                    || self.display_labels.paused
                    || !current
                    || !active
                {
                    if active {
                        self.display_labels.retry_after_drained_reply();
                    }
                    return true;
                }
                self.display_labels
                    .fail_request(&request, format!("{error}"));
                true
            }
            _ => false,
        }
    }
}

fn same_lifecycle(left: &DisplayLabelsContext, right: &DisplayLabelsContext) -> bool {
    left.source_id == right.source_id && left.lifecycle_generation == right.lifecycle_generation
}

/// Check pending curation only for the source lifecycle represented by a label context.
pub(super) fn curation_pending_for_context(
    curation_state: &super::curation::CurationState,
    context: &DisplayLabelsContext,
) -> bool {
    curation_state.has_pending_for_lifecycle(&super::curation::CurationLifecycleKey {
        source_id: context.source_id.clone(),
        lifecycle_generation: context.lifecycle_generation,
    })
}

fn contexts_match(
    left: Option<&DisplayLabelsContext>,
    right: Option<&DisplayLabelsContext>,
) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return left.is_none() && right.is_none();
    };
    left.source_id == right.source_id
        && left.lifecycle_generation == right.lifecycle_generation
        && left.root == right.root
        && left.root_identity == right.root_identity
        && left.directory_generation == right.directory_generation
        && left.revision == right.revision
        && Arc::ptr_eq(&left.entry_indices, &right.entry_indices)
}

fn context_is_active(context: &DisplayLabelsContext, browser: &Browser) -> bool {
    browser.active_source.is_some_and(|index| {
        browser.source_sessions.get(index).is_some_and(|session| {
            session.source_id.as_ref() == Some(&context.source_id)
                && session.root == context.root
                && session.catalog_lifecycle_generation == Some(context.lifecycle_generation)
        })
    }) && browser
        .catalog_activation
        .as_ref()
        .is_some_and(|activation| {
            activation.source_id == context.source_id
                && activation.lifecycle_generation == context.lifecycle_generation
                && activation.root == context.root
                && activation.root_identity.as_ref() == Some(&context.root_identity)
        })
}

impl DisplayLabelsRequest {
    fn into_command(self) -> CatalogCommand {
        CatalogCommand::RequestDisplayLabels {
            source_id: self.context.source_id,
            lifecycle_generation: self.context.lifecycle_generation,
            expected_revision: self.context.revision,
            request_id: self.request_id,
            relative_paths: self.paths,
        }
    }
}

pub(super) fn display_context_from_metadata(
    context: catalog_metadata::CatalogMetadataContext,
) -> DisplayLabelsContext {
    DisplayLabelsContext {
        source_id: context.source_id,
        lifecycle_generation: context.lifecycle_generation,
        root: context.root,
        root_identity: context.root_identity,
        directory_generation: context.directory_generation,
        revision: context.revision,
        entry_indices: context.entry_indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(paths: &[&str]) -> DisplayLabelsContext {
        DisplayLabelsContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/library"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 2,
            revision: 7,
            entry_indices: Arc::new(
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| (PathBuf::from(path), index))
                    .collect(),
            ),
        }
    }

    fn entry(path: &str, label: Option<String>) -> SourceDisplayLabelEntry {
        SourceDisplayLabelEntry {
            relative_path: PathBuf::from(path),
            display_label: label,
            truncated: false,
        }
    }

    fn cache_entry(state: &mut DisplayLabelsState, context: &DisplayLabelsContext, path: &str) {
        assert!(state.observe_visible_demand(context.clone(), vec![PathBuf::from(path)],));
        let request = state.begin_request().expect("cache request admitted");
        let request = state
            .take_pending(request.request_id)
            .expect("cache request remains pending");
        state
            .apply_ready(
                &request,
                &request.paths,
                &[entry(path, Some(String::from("cached")))],
            )
            .expect("cache response is valid");
    }

    #[test]
    fn visible_demand_is_deferred_and_latest_wins_around_one_pending_request() {
        let context = context(&["a.wav", "b.wav", "c.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        assert!(state.observe_visible_demand(
            context.clone(),
            vec![PathBuf::from("a.wav"), PathBuf::from("b.wav")],
        ));
        assert!(state.pending.is_none());
        let first = state.begin_request().expect("first demand is admitted");
        assert_eq!(
            first.paths,
            [PathBuf::from("a.wav"), PathBuf::from("b.wav")]
        );
        assert!(!state.observe_visible_demand(
            context.clone(),
            vec![PathBuf::from("c.wav"), PathBuf::from("b.wav")],
        ));
        assert!(state.begin_request().is_none());
        let first = state
            .take_pending(first.request_id)
            .expect("first request remains pending");
        state
            .apply_ready(
                &first,
                &first.paths,
                &[
                    entry("a.wav", Some(String::from("A"))),
                    entry("b.wav", None),
                ],
            )
            .expect("first response is valid");
        let second = state.begin_request().expect("latest demand is admitted");
        assert_eq!(second.paths, [PathBuf::from("c.wav")]);
    }

    #[test]
    fn exact_context_and_requested_paths_fence_replies() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]);
        let request = state.begin_request().expect("request admitted");
        let other_root = RootIdentity::from_bytes(b"other".to_vec());
        assert!(
            state
                .ready_request(
                    &request.context.source_id,
                    request.context.lifecycle_generation,
                    &other_root,
                    request.context.directory_generation,
                    request.context.revision,
                    request.request_id,
                    &request.paths,
                )
                .is_none()
        );
        assert!(
            state
                .ready_request(
                    &request.context.source_id,
                    request.context.lifecycle_generation,
                    &request.context.root_identity,
                    request.context.directory_generation,
                    request.context.revision,
                    request.request_id,
                    &[PathBuf::from("other.wav")],
                )
                .is_none()
        );
        assert!(
            state
                .ready_request(
                    &request.context.source_id,
                    request.context.lifecycle_generation,
                    &request.context.root_identity,
                    request.context.directory_generation,
                    request.context.revision,
                    request.request_id,
                    &request.paths,
                )
                .is_some()
        );
    }

    #[test]
    fn missing_entries_are_terminal_fallbacks_without_idle_retries() {
        let context = context(&["missing.wav", "known.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(
            context,
            vec![PathBuf::from("missing.wav"), PathBuf::from("known.wav")],
        );
        let request = state.begin_request().expect("request admitted");
        let request = state
            .take_pending(request.request_id)
            .expect("request remains pending");
        state
            .apply_ready(
                &request,
                &request.paths,
                &[entry("known.wav", Some(String::from("known")))],
            )
            .expect("omitted path is valid fallback");
        assert!(state.terminal_paths.contains(&PathBuf::from("missing.wav")));
        assert!(!state.poll_needed());
        assert!(state.begin_request().is_none());
    }

    #[test]
    fn successive_visible_windows_keep_labels_bounded_and_hydrated() {
        let paths = (0..320)
            .map(|index| format!("{index}.wav"))
            .collect::<Vec<_>>();
        let refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let context = context(&refs);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        let label = "x".repeat(SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES);
        for window in paths.chunks(SOURCE_DISPLAY_LABEL_MAX_PATHS) {
            state.observe_visible_demand(
                context.clone(),
                window.iter().map(PathBuf::from).collect::<Vec<_>>(),
            );
            while state.poll_needed() {
                let request = state.begin_request().expect("next cache batch");
                let request = state
                    .take_pending(request.request_id)
                    .expect("cache batch remains pending");
                let entries = request
                    .paths
                    .iter()
                    .map(|path| entry(path.to_string_lossy().as_ref(), Some(label.clone())))
                    .collect::<Vec<_>>();
                state
                    .apply_ready(&request, &request.paths, &entries)
                    .expect("bounded response");
            }
            assert_eq!(state.cache.len(), window.len());
            assert!(
                window
                    .iter()
                    .all(|path| state.cache.contains_key(Path::new(path)))
            );
            assert!(state.terminal_paths.len() <= MAX_VISIBLE_DEMAND_PATHS);
            assert!(state.cache_label_bytes <= DISPLAY_LABEL_CACHE_MAX_LABEL_BYTES);
            assert!(state.cache_path_bytes <= DISPLAY_LABEL_CACHE_MAX_PATH_BYTES);
            assert_eq!(
                state.cache_label_bytes,
                state
                    .cache
                    .values()
                    .map(|cached| cached.label.as_ref().map_or(0, String::len))
                    .sum::<usize>()
            );
            assert_eq!(
                state.cache_path_bytes,
                state
                    .cache
                    .keys()
                    .map(|path| path_byte_len(path))
                    .sum::<usize>()
            );
        }
        assert!(!state.cache.contains_key(&PathBuf::from("0.wav")));
        assert!(!state.poll_needed());
    }

    #[test]
    fn request_batches_by_path_bytes_before_the_sixty_four_path_limit() {
        let paths = (0..64)
            .map(|index| format!("{index}-{}", "x".repeat(5_000)))
            .collect::<Vec<_>>();
        let refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let context = context(&refs);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, paths.iter().map(PathBuf::from).collect());
        let first = state.begin_request().expect("first byte-bounded batch");
        let first_bytes: usize = first
            .paths
            .iter()
            .map(|path| request_path_byte_len(path).expect("valid path"))
            .sum();
        assert!(first.paths.len() < SOURCE_DISPLAY_LABEL_MAX_PATHS);
        assert!(first_bytes <= SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES);
        let first = state
            .take_pending(first.request_id)
            .expect("first batch remains pending");
        let entries = first
            .paths
            .iter()
            .map(|path| entry(path.to_string_lossy().as_ref(), Some(String::from("label"))))
            .collect::<Vec<_>>();
        state
            .apply_ready(&first, &first.paths, &entries)
            .expect("first byte-bounded response");
        let mut admitted = first.paths.len();
        while state.poll_needed() {
            let second = state.begin_request().expect("remaining paths are admitted");
            let second_bytes: usize = second
                .paths
                .iter()
                .map(|path| request_path_byte_len(path).expect("valid path"))
                .sum();
            assert!(second_bytes <= SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES);
            admitted += second.paths.len();
            let second = state
                .take_pending(second.request_id)
                .expect("next batch remains pending");
            let entries = second
                .paths
                .iter()
                .map(|path| entry(path.to_string_lossy().as_ref(), Some(String::from("label"))))
                .collect::<Vec<_>>();
            state
                .apply_ready(&second, &second.paths, &entries)
                .expect("next byte-bounded response");
        }
        assert_eq!(admitted, paths.len());
        assert!(!state.poll_needed());
    }

    #[test]
    fn same_lifecycle_projection_retain_pending_until_stale_reply_is_drained() {
        let old_context = context(&["old.wav", "new.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(old_context.clone());
        state.observe_visible_demand(old_context.clone(), vec![PathBuf::from("old.wav")]);
        let old_request = state.begin_request().expect("old request admitted");
        let new_context = DisplayLabelsContext {
            revision: old_context.revision + 1,
            entry_indices: Arc::new(HashMap::from([
                (PathBuf::from("old.wav"), 0),
                (PathBuf::from("new.wav"), 1),
            ])),
            ..old_context.clone()
        };
        state.install_projection(new_context.clone());
        assert!(state.pending.is_some());
        let old_request = state
            .take_pending(old_request.request_id)
            .expect("stale request remains drainable");
        state
            .apply_ready(
                &old_request,
                &old_request.paths,
                &[entry("old.wav", Some(String::from("old")))],
            )
            .expect("stale payload shape remains valid");
        assert!(state.cache.is_empty());
        state.observe_visible_demand(new_context, vec![PathBuf::from("new.wav")]);
        assert!(state.begin_request().is_some());
    }

    #[test]
    fn non_tag_curation_rebases_cached_labels_and_retries_after_old_reply_drains() {
        let old_context = context(&["cached.wav", "pending.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(old_context.clone());
        cache_entry(&mut state, &old_context, "cached.wav");
        state.observe_visible_demand(
            old_context.clone(),
            vec![PathBuf::from("cached.wav"), PathBuf::from("pending.wav")],
        );
        let old_request = state.begin_request().expect("old request admitted");
        state.pause_for_curation(false);
        assert!(state.paused);
        assert!(state.cache.contains_key(&PathBuf::from("cached.wav")));
        let new_context = DisplayLabelsContext {
            revision: old_context.revision + 1,
            ..old_context.clone()
        };
        assert!(state.rebase_after_curation(
            new_context.clone(),
            old_context.revision,
            new_context.revision,
            &new_context.root_identity,
            new_context.directory_generation,
            false,
        ));
        assert!(!state.paused);
        assert!(state.cache.contains_key(&PathBuf::from("cached.wav")));
        let old_request = state
            .take_pending(old_request.request_id)
            .expect("old revision reply remains drainable");
        assert_eq!(old_request.context.revision, old_context.revision);
        assert!(!state.cache.contains_key(&PathBuf::from("pending.wav")));
        state.retry_after_drained_reply();
        let retry = state
            .begin_request()
            .expect("new revision request admitted");
        assert_eq!(retry.context.revision, new_context.revision);
        assert_eq!(retry.paths, [PathBuf::from("pending.wav")]);
    }

    #[test]
    fn tag_or_uncertain_curation_clears_cached_labels() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        cache_entry(&mut state, &context, "sample.wav");
        assert!(state.cache.contains_key(&PathBuf::from("sample.wav")));
        state.pause_for_curation(true);
        assert!(state.paused);
        assert!(state.cache.is_empty());
        assert!(state.latest_demand.is_empty());
        assert!(state.terminal_paths.is_empty());
    }

    #[test]
    fn non_tag_noop_receipt_rebases_without_clearing_cached_labels() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        cache_entry(&mut state, &context, "sample.wav");
        state.pause_for_curation(false);
        assert!(state.rebase_after_curation(
            context.clone(),
            context.revision,
            context.revision,
            &context.root_identity,
            context.directory_generation,
            false,
        ));
        assert!(state.cache.contains_key(&PathBuf::from("sample.wav")));
        assert!(!state.paused);
    }

    #[test]
    fn malformed_ready_payload_is_rejected_before_cache_mutation() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]);
        let request = state.begin_request().expect("request admitted");
        let request = state
            .take_pending(request.request_id)
            .expect("request remains pending");
        let malformed = SourceDisplayLabelEntry {
            relative_path: PathBuf::from("other.wav"),
            display_label: Some(String::from("wrong")),
            truncated: false,
        };
        assert!(
            state
                .apply_ready(&request, &request.paths, &[malformed])
                .is_err()
        );
        assert!(state.cache.is_empty());
    }

    #[test]
    fn terminal_admission_failure_does_not_repeat_the_same_paths() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]);
        let request = state.begin_request().expect("request admitted");
        assert!(state.admission_failed(request.request_id, false, Some(String::from("bad"))));
        assert!(!state.poll_needed());
        assert!(state.begin_request().is_none());
    }

    #[test]
    fn disabling_label_mode_cancels_queued_work_but_drains_admitted_work() {
        let context = context(&["sample.wav"]);
        let mut state = DisplayLabelsState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]);
        let request = state.begin_request().expect("request admitted");
        state.disable_mode();
        assert!(!state.retry_ready);
        assert!(state.pending.is_some());
        let request = state
            .take_pending(request.request_id)
            .expect("admitted request remains drainable");
        state
            .apply_ready(&request, &request.paths, &[])
            .expect("reply remains consumable");
        assert!(!state.poll_needed());
    }
}
