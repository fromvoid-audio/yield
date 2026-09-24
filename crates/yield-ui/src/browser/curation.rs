#![deny(missing_docs)]
#![deny(warnings)]

//! Passive curation metadata projection and source-scoped collection filters.

use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use gpui::{ClickEvent, Context, ScrollStrategy, div, prelude::*, px};
use yield_core::{
    AudioFile, AudioFileCuration, AudioFileId, BrowserViewModel, COLLECTION_COUNT, CurationAction,
    CurationTransition, MAX_TAGS_PER_COMMIT, PLAYBACK_TYPE_FILTERS, PlaybackType,
    PlaybackTypeFilter, RATING_FILTER_LEVELS, RatingFilter, TagFilterMask, plan_curation_action,
};
use yield_design::Theme;
use yield_library::sample_sources::{
    BrowserFileMetadata, Rating, SOURCE_CURATION_MAX_TAG_LABEL_BYTES, SampleCollection,
    SourceCurationUpdate, SourceId, SourcePlaybackType,
    reconciliation::{RootIdentity, RootRelativePath},
};
use yield_session::{
    CatalogPublicationStatus, MAX_CURATION_OPERATION_BATCHES, MAX_CURATION_OPERATION_PATH_BYTES,
    MAX_CURATION_OPERATION_RECEIPT_BYTES, MAX_CURATION_OPERATION_UPDATES, MAX_CURATION_UPDATES,
    SourceCurationCommitReceipt, SourceCurationOperationFence, SourceCurationOperationOutcome,
};

use crate::theme::{color, transparent};

use super::{Browser, catalog, tag_chips, tag_filter};

const COMPACT_FILTER_LABEL_WIDTH: f32 = 82.0;

fn compact_filter_chip_width(theme: &Theme, count: usize) -> f32 {
    let count = count.max(1) as f32;
    let available = theme.spacing.sidebar_width
        - theme.spacing.panel_padding * 2.0
        - COMPACT_FILTER_LABEL_WIDTH
        - theme.spacing.control_gap * count;
    (available / count).max(theme.spacing.border_width)
}

fn compact_filter_row<E>(theme: &Theme, label: &'static str, controls: Vec<E>) -> gpui::Div
where
    E: gpui::IntoElement,
{
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
                .flex_none()
                .w_full()
                .h(px(theme.spacing.row_height))
                .items_center()
                .px(px(theme.spacing.panel_padding))
                .gap(px(theme.spacing.control_gap))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .w(px(COMPACT_FILTER_LABEL_WIDTH))
                        .h_full()
                        .items_center()
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(theme.colors.accent))
                        .child(label),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .items_center()
                        .gap(px(theme.spacing.control_gap))
                        .children(controls),
                ),
        )
}

/// The source lifecycle identity retained by curation state.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct CurationLifecycleKey {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
}

#[derive(Clone, Debug)]
struct PendingCurationOperation {
    request_id: u64,
    batch_count: usize,
    tags_may_change: bool,
    overlays: BTreeMap<PathBuf, AudioFileCuration>,
    active_overlay: bool,
    trash_required: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TagMutation {
    Add,
    AddExisting { tag_id: i64 },
    Remove,
    RemoveExisting { tag_id: i64 },
}

#[derive(Clone, Debug)]
struct CurationLineage {
    root_identity: RootIdentity,
    directory_generation: u64,
    revision: u64,
    confirmed: BTreeMap<PathBuf, AudioFileCuration>,
    pending: Vec<PendingCurationOperation>,
    failed: Option<String>,
    /// A committed receipt was drained after its source fence became stale.
    invalidated: Option<String>,
    message: Option<String>,
}

/// Browser-owned curation state for accepted operations and their optimistic overlays.
///
/// The catalog remains the owner of durable writes. This state only retains enough information
/// to render accepted pending outcomes and to reapply newer overlays when an earlier receipt
/// arrives.
#[derive(Default)]
pub(super) struct CurationState {
    next_request_id: u64,
    lineages: HashMap<CurationLifecycleKey, CurationLineage>,
    retired_pending: HashSet<(CurationLifecycleKey, u64)>,
}

/// One operation ready for immediate catalog admission.
pub(super) struct CurationOperationPlan {
    pub(super) request_id: u64,
    pub(super) fence: SourceCurationOperationFence,
    pub(super) root_identity: RootIdentity,
    pub(super) batches: Vec<Vec<SourceCurationUpdate>>,
    overlays: BTreeMap<PathBuf, AudioFileCuration>,
    trash_required: usize,
}

impl CurationOperationPlan {
    fn tags_may_change(&self) -> bool {
        self.batches
            .iter()
            .flatten()
            .any(source_update_may_change_tags)
    }
}

/// Pure result of planning one browser action across its selected target paths.
pub(super) enum CurationPlan {
    /// Every target was unchanged; `trash_required` records fourth-negative rows that need a
    /// future filesystem transaction.
    NoChange { trash_required: usize },
    /// The operation can be submitted immediately.
    Ready(CurationOperationPlan),
    /// The current projection cannot safely admit this operation.
    Rejected(String),
}

/// Admission result for a browser tag operation.
pub(super) enum TagAdmissionResult {
    /// The catalog accepted a durable tag operation.
    Accepted,
    /// The planner produced no durable updates for the captured targets.
    NoChange,
    /// The operation could not be admitted with the captured curation authority.
    Rejected(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CurationReceipt {
    source_revision: u64,
    root_identity: RootIdentity,
    directory_generation: u64,
    files: BTreeMap<PathBuf, AudioFileCuration>,
    playback_types: BTreeMap<PathBuf, Option<PlaybackType>>,
    valid: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CurationOutcome {
    initial_revision: Option<u64>,
    final_revision: Option<u64>,
    committed_batches: Vec<CurationReceipt>,
    failure: Option<String>,
}

/// Applied state returned after one terminal catalog outcome is consumed.
pub(super) struct CurationOutcomeApplication {
    pub(super) root_identity: RootIdentity,
    pub(super) directory_generation: u64,
    pub(super) prior_revision: u64,
    pub(super) revision: u64,
    pub(super) visible: BTreeMap<PathBuf, AudioFileCuration>,
    pub(super) playback_types: BTreeMap<PathBuf, Option<PlaybackType>>,
    pub(super) metadata_changed: bool,
    pub(super) tags_may_change: bool,
    pub(super) failed: bool,
    /// Whether the receipt may update the active browser projection.
    pub(super) projectable: bool,
}

impl CurationState {
    /// Install a fresh audited projection, replacing any older mapping for this lifecycle.
    pub(super) fn install_projection(
        &mut self,
        key: CurationLifecycleKey,
        root_identity: RootIdentity,
        directory_generation: u64,
        revision: u64,
        entries: &[AudioFile],
    ) {
        if let Some(previous) = self.lineages.remove(&key) {
            self.retired_pending.extend(
                previous
                    .pending
                    .into_iter()
                    .map(|operation| (key.clone(), operation.request_id)),
            );
        }
        let confirmed = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .curation
                    .map(|curation| (entry.relative_path.clone(), curation))
            })
            .collect();
        self.lineages.insert(
            key,
            CurationLineage {
                root_identity,
                directory_generation,
                revision,
                confirmed,
                pending: Vec::new(),
                failed: None,
                invalidated: None,
                message: None,
            },
        );
    }

    /// Return whether any accepted operation is still awaiting a terminal outcome.
    pub(super) fn has_pending_outcomes(&self) -> bool {
        self.lineages
            .values()
            .any(|lineage| !lineage.pending.is_empty())
            || !self.retired_pending.is_empty()
    }

    /// Return whether one source lifecycle has accepted operations awaiting terminal outcomes.
    pub(super) fn has_pending(&self, key: &CurationLifecycleKey) -> bool {
        self.lineages
            .get(key)
            .is_some_and(|lineage| !lineage.pending.is_empty())
    }

    /// Return whether a source lifecycle still has an accepted operation in flight, including
    /// operations retained after its active projection was replaced.
    pub(super) fn has_pending_for_lifecycle(&self, key: &CurationLifecycleKey) -> bool {
        self.has_pending(key)
            || self
                .retired_pending
                .iter()
                .any(|(retired_key, _)| retired_key == key)
    }

    /// Verify that a browser metadata context still names this exact curation lineage.
    pub(super) fn authority_matches(
        &self,
        key: &CurationLifecycleKey,
        root_identity: &RootIdentity,
        directory_generation: u64,
        revision: u64,
    ) -> bool {
        self.lineages.get(key).is_some_and(|lineage| {
            lineage.root_identity == *root_identity
                && lineage.directory_generation == directory_generation
                && lineage.revision == revision
                && lineage.failed.is_none()
                && lineage.invalidated.is_none()
        })
    }

    /// Return a source-specific status suitable for the browser status strip.
    pub(super) fn status_for(&self, key: &CurationLifecycleKey) -> Option<String> {
        let lineage = self.lineages.get(key)?;
        if let Some(error) = lineage.failed.as_ref() {
            return Some(format!(
                "Curation failed: {error}; refresh the catalog before editing"
            ));
        }
        if let Some(reason) = lineage.invalidated.as_ref() {
            return Some(format!(
                "Curation committed but cannot update this source: {reason}; refresh the catalog before editing"
            ));
        }
        if !lineage.pending.is_empty() {
            let trash_required = lineage
                .pending
                .iter()
                .map(|operation| operation.trash_required)
                .sum::<usize>();
            let trash_suffix = (trash_required > 0).then(|| {
                format!(
                    "; trash unavailable for {} file{}",
                    trash_required,
                    if trash_required == 1 { "" } else { "s" }
                )
            });
            return Some(format!(
                "Saving curation… ({} operation{} pending){}",
                lineage.pending.len(),
                if lineage.pending.len() == 1 { "" } else { "s" },
                trash_suffix.unwrap_or_default()
            ));
        }
        lineage.message.clone()
    }

    /// Set a source-specific presentation message without changing durable or optimistic state.
    pub(super) fn set_message(&mut self, key: &CurationLifecycleKey, message: String) {
        if let Some(lineage) = self.lineages.get_mut(key) {
            lineage.message = Some(message);
        }
    }

    /// Drop completed inactive lifecycle mappings while retaining accepted outcomes still in
    /// flight and the active lifecycle's confirmed metadata.
    pub(super) fn prune_completed_except(&mut self, active: Option<&CurationLifecycleKey>) {
        self.lineages
            .retain(|key, lineage| active == Some(key) || !lineage.pending.is_empty());
    }

    /// Plan one action from the current confirmed state plus accepted optimistic overlays.
    pub(super) fn plan_action(
        &self,
        key: &CurationLifecycleKey,
        action: CurationAction,
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        let Some(lineage) = self.lineages.get(key) else {
            return CurationPlan::Rejected(String::from(
                "Curation requires a successful audited catalog projection",
            ));
        };
        if let Some(error) = lineage.failed.as_ref() {
            return CurationPlan::Rejected(format!(
                "Curation lineage failed ({error}); refresh the catalog before editing"
            ));
        }
        if let Some(reason) = lineage.invalidated.as_ref() {
            return CurationPlan::Rejected(format!(
                "Curation lineage is unavailable ({reason}); refresh the catalog before editing"
            ));
        }
        if target_paths.is_empty() {
            return CurationPlan::NoChange { trash_required: 0 };
        }
        let mut seen = HashSet::with_capacity(target_paths.len());
        let mut rows = Vec::new();
        let mut trash_required = 0;
        for path in target_paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(current) = effective_curation(lineage, path) else {
                return CurationPlan::Rejected(format!(
                    "Curation metadata is unavailable for {}",
                    path.display()
                ));
            };
            match plan_curation_action(current, action) {
                CurationTransition::Unchanged => {}
                CurationTransition::RequiresTrash => trash_required += 1,
                CurationTransition::Changed(after) => {
                    let Ok(relative_path) = RootRelativePath::try_from_path(path.clone()) else {
                        return CurationPlan::Rejected(format!(
                            "Invalid source-relative curation path {}",
                            path.display()
                        ));
                    };
                    let updates = updates_for_action(current, after, action, relative_path);
                    if updates.is_empty() {
                        continue;
                    }
                    rows.push((path.clone(), after, updates));
                }
            }
        }
        if rows.is_empty() {
            return CurationPlan::NoChange { trash_required };
        }

        let mut batches = Vec::new();
        let mut batch = Vec::new();
        let mut path_bytes = 0usize;
        let mut overlays = BTreeMap::new();
        for (path, after, updates) in rows {
            if updates.len() > MAX_CURATION_UPDATES {
                return CurationPlan::Rejected(format!(
                    "One curation row exceeds the {} update bound",
                    MAX_CURATION_UPDATES
                ));
            }
            if !batch.is_empty() && batch.len() + updates.len() > MAX_CURATION_UPDATES {
                batches.push(std::mem::take(&mut batch));
            }
            path_bytes =
                path_bytes.saturating_add(path_byte_len(&path).saturating_mul(updates.len()));
            batch.extend(updates);
            overlays.insert(path, after);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        let total_updates = batches.iter().map(Vec::len).sum::<usize>();
        if batches.len() > MAX_CURATION_OPERATION_BATCHES {
            return CurationPlan::Rejected(format!(
                "Selection needs {} curation batches; maximum is {}",
                batches.len(),
                MAX_CURATION_OPERATION_BATCHES
            ));
        }
        if total_updates > MAX_CURATION_OPERATION_UPDATES {
            return CurationPlan::Rejected(format!(
                "Selection needs {} curation updates; maximum is {}",
                total_updates, MAX_CURATION_OPERATION_UPDATES
            ));
        }
        if path_bytes > MAX_CURATION_OPERATION_PATH_BYTES {
            return CurationPlan::Rejected(format!(
                "Selection paths exceed the {} byte curation bound",
                MAX_CURATION_OPERATION_PATH_BYTES
            ));
        }
        let estimated_payload = path_bytes
            .saturating_mul(2)
            .saturating_add(
                lineage
                    .root_identity
                    .as_bytes()
                    .len()
                    .saturating_mul(batches.len() + 1),
            )
            .saturating_add(total_updates.saturating_mul(256))
            .saturating_add(batches.len().saturating_mul(64));
        if estimated_payload > MAX_CURATION_OPERATION_RECEIPT_BYTES {
            return CurationPlan::Rejected(format!(
                "Selection curation payload is {} bytes; maximum is {}",
                estimated_payload, MAX_CURATION_OPERATION_RECEIPT_BYTES
            ));
        }
        let request_id = self.next_request_id();
        let fence = lineage
            .pending
            .last()
            .map(|pending| SourceCurationOperationFence::AfterOperation {
                request_id: pending.request_id,
            })
            .unwrap_or(SourceCurationOperationFence::ExactRevision(
                lineage.revision,
            ));
        CurationPlan::Ready(CurationOperationPlan {
            request_id,
            fence,
            root_identity: lineage.root_identity.clone(),
            batches,
            overlays,
            trash_required,
        })
    }

    /// Plan ordered tag assignments or removals for the selected source-relative paths.
    ///
    /// Tag operations deliberately carry no optimistic curation overlay. Their receipts are the
    /// first browser-visible source of both curation and playback-type changes.
    fn plan_tag_operation(
        &self,
        key: &CurationLifecycleKey,
        mutation: TagMutation,
        labels: &[String],
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        let Some(lineage) = self.lineages.get(key) else {
            return CurationPlan::Rejected(String::from(
                "Curation requires a successful audited catalog projection",
            ));
        };
        if let Some(error) = lineage.failed.as_ref() {
            return CurationPlan::Rejected(format!(
                "Curation lineage failed ({error}); refresh the catalog before editing"
            ));
        }
        if let Some(reason) = lineage.invalidated.as_ref() {
            return CurationPlan::Rejected(format!(
                "Curation lineage is unavailable ({reason}); refresh the catalog before editing"
            ));
        }
        if target_paths.is_empty() {
            return CurationPlan::NoChange { trash_required: 0 };
        }
        let label_capacity = labels.len().min(MAX_TAGS_PER_COMMIT);
        let mut seen_labels = HashSet::with_capacity(label_capacity);
        let mut unique_labels = Vec::with_capacity(label_capacity);
        let label_count = match mutation {
            TagMutation::AddExisting { tag_id } | TagMutation::RemoveExisting { tag_id } => {
                if tag_id <= 0 {
                    return CurationPlan::Rejected(String::from(
                        "Existing tag id must be positive",
                    ));
                }
                1
            }
            TagMutation::Add | TagMutation::Remove => {
                if labels.is_empty() {
                    return CurationPlan::Rejected(String::from(
                        "Tag operation requires at least one label",
                    ));
                }
                for label in labels {
                    if label.len() > SOURCE_CURATION_MAX_TAG_LABEL_BYTES {
                        return CurationPlan::Rejected(format!(
                            "Tag label is {} bytes; maximum is {}",
                            label.len(),
                            SOURCE_CURATION_MAX_TAG_LABEL_BYTES
                        ));
                    }
                    if label.contains('\0') {
                        return CurationPlan::Rejected(String::from(
                            "Tag labels cannot contain an embedded NUL byte",
                        ));
                    }
                    if label.split_whitespace().next().is_none() {
                        return CurationPlan::Rejected(String::from(
                            "Tag operation requires non-empty labels",
                        ));
                    }
                    if seen_labels.insert(label.as_str()) {
                        if unique_labels.len() >= MAX_TAGS_PER_COMMIT {
                            return CurationPlan::Rejected(format!(
                                "Tag operation contains more than {} distinct labels",
                                MAX_TAGS_PER_COMMIT
                            ));
                        }
                        unique_labels.push(label.as_str());
                    }
                }
                if unique_labels.is_empty() {
                    return CurationPlan::Rejected(String::from(
                        "Tag operation requires at least one label",
                    ));
                }
                unique_labels.len()
            }
        };
        let max_distinct_paths = MAX_CURATION_OPERATION_UPDATES / label_count;
        let mut seen_paths = HashSet::with_capacity(target_paths.len().min(max_distinct_paths));
        let mut paths = Vec::with_capacity(target_paths.len().min(max_distinct_paths));
        let mut path_bytes = 0usize;
        for path in target_paths {
            if !seen_paths.insert(path.as_path()) {
                continue;
            }
            if paths.len() >= max_distinct_paths {
                return CurationPlan::Rejected(format!(
                    "Tag selection needs more than {} updates",
                    MAX_CURATION_OPERATION_UPDATES
                ));
            }
            if effective_curation(lineage, path).is_none() {
                return CurationPlan::Rejected(format!(
                    "Curation metadata is unavailable for {}",
                    path.display()
                ));
            }
            let path_bytes_for_row = match path_byte_len(path).checked_mul(label_count) {
                Some(path_bytes) => path_bytes,
                None => {
                    return CurationPlan::Rejected(String::from(
                        "Tag selection path payload exceeds the curation byte bound",
                    ));
                }
            };
            path_bytes = match path_bytes.checked_add(path_bytes_for_row) {
                Some(path_bytes) => path_bytes,
                None => {
                    return CurationPlan::Rejected(String::from(
                        "Tag selection path payload exceeds the curation byte bound",
                    ));
                }
            };
            if path_bytes > MAX_CURATION_OPERATION_PATH_BYTES {
                return CurationPlan::Rejected(format!(
                    "Selection paths exceed the {} byte curation bound",
                    MAX_CURATION_OPERATION_PATH_BYTES
                ));
            }
            paths.push(path.as_path());
        }
        if paths.is_empty() {
            return CurationPlan::NoChange { trash_required: 0 };
        }

        let path_count = paths.len();
        let total_updates = match path_count.checked_mul(label_count) {
            Some(total_updates) => total_updates,
            None => {
                return CurationPlan::Rejected(String::from(
                    "Tag selection exceeds the curation update bound",
                ));
            }
        };
        if total_updates > MAX_CURATION_OPERATION_UPDATES {
            return CurationPlan::Rejected(format!(
                "Tag selection needs {} updates; maximum is {}",
                total_updates, MAX_CURATION_OPERATION_UPDATES
            ));
        }

        let label_bytes = match mutation {
            TagMutation::AddExisting { .. } | TagMutation::RemoveExisting { .. } => 0,
            TagMutation::Add | TagMutation::Remove => match unique_labels
                .iter()
                .try_fold(0usize, |total, label| total.checked_add(label.len()))
            {
                Some(label_bytes) => label_bytes,
                None => {
                    return CurationPlan::Rejected(String::from(
                        "Tag label payload exceeds the curation byte bound",
                    ));
                }
            },
        };
        let label_bytes = match label_bytes.checked_mul(path_count) {
            Some(label_bytes) => label_bytes,
            None => {
                return CurationPlan::Rejected(String::from(
                    "Tag label payload exceeds the curation byte bound",
                ));
            }
        };

        // Every path's labels stay together. The native update bound permits at least two rows
        // for the maximum label count, so this calculation is always nonzero after validation.
        let paths_per_batch = MAX_CURATION_UPDATES / label_count;
        let batch_count = (path_count - 1) / paths_per_batch + 1;
        if batch_count > MAX_CURATION_OPERATION_BATCHES {
            return CurationPlan::Rejected(format!(
                "Selection needs {} curation batches; maximum is {}",
                batch_count, MAX_CURATION_OPERATION_BATCHES
            ));
        }
        let estimated_payload = path_bytes
            .saturating_mul(2)
            .saturating_add(
                lineage
                    .root_identity
                    .as_bytes()
                    .len()
                    .saturating_mul(batch_count + 1),
            )
            .saturating_add(total_updates.saturating_mul(256))
            .saturating_add(match mutation {
                TagMutation::AddExisting { .. } | TagMutation::RemoveExisting { .. } => {
                    total_updates.saturating_mul(std::mem::size_of::<i64>())
                }
                TagMutation::Add | TagMutation::Remove => 0,
            })
            .saturating_add(label_bytes)
            .saturating_add(batch_count.saturating_mul(64));
        if estimated_payload > MAX_CURATION_OPERATION_RECEIPT_BYTES {
            return CurationPlan::Rejected(format!(
                "Tag selection payload is {} bytes; maximum is {}",
                estimated_payload, MAX_CURATION_OPERATION_RECEIPT_BYTES
            ));
        }

        let mut relative_paths = Vec::with_capacity(path_count);
        for path in paths {
            let Ok(relative_path) = RootRelativePath::try_from_path(path.to_path_buf()) else {
                return CurationPlan::Rejected(format!(
                    "Invalid source-relative curation path {}",
                    path.display()
                ));
            };
            relative_paths.push(relative_path);
        }

        let mut batches = Vec::with_capacity(batch_count);
        let mut batch = Vec::with_capacity((paths_per_batch * label_count).min(total_updates));
        for relative_path in relative_paths {
            if !batch.is_empty() && batch.len() + label_count > MAX_CURATION_UPDATES {
                batches.push(std::mem::take(&mut batch));
                batch.reserve(paths_per_batch * label_count);
            }
            match mutation {
                TagMutation::AddExisting { tag_id } => {
                    batch.push(SourceCurationUpdate::AssignExistingTag {
                        path: relative_path,
                        tag_id,
                    });
                }
                TagMutation::RemoveExisting { tag_id } => {
                    batch.push(SourceCurationUpdate::RemoveExistingTag {
                        path: relative_path,
                        tag_id,
                    });
                }
                TagMutation::Add | TagMutation::Remove => {
                    for label in &unique_labels {
                        let update = match mutation {
                            TagMutation::Add => SourceCurationUpdate::AddTag {
                                path: relative_path.clone(),
                                label: (*label).to_owned(),
                            },
                            TagMutation::Remove => SourceCurationUpdate::RemoveTag {
                                path: relative_path.clone(),
                                label: (*label).to_owned(),
                            },
                            TagMutation::AddExisting { .. }
                            | TagMutation::RemoveExisting { .. } => {
                                unreachable!("existing-tag update was handled above")
                            }
                        };
                        batch.push(update);
                    }
                }
            }
        }
        if !batch.is_empty() {
            batches.push(batch);
        }

        let request_id = self.next_request_id();
        let fence = lineage
            .pending
            .last()
            .map(|pending| SourceCurationOperationFence::AfterOperation {
                request_id: pending.request_id,
            })
            .unwrap_or(SourceCurationOperationFence::ExactRevision(
                lineage.revision,
            ));
        CurationPlan::Ready(CurationOperationPlan {
            request_id,
            fence,
            root_identity: lineage.root_identity.clone(),
            batches,
            overlays: BTreeMap::new(),
            trash_required: 0,
        })
    }

    /// Plan one or more ordered tag additions without creating optimistic tag vectors.
    pub(super) fn plan_add_tags(
        &self,
        key: &CurationLifecycleKey,
        labels: &[String],
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        self.plan_tag_operation(key, TagMutation::Add, labels, target_paths)
    }

    /// Plan one exact tag removal without creating optimistic tag vectors.
    pub(super) fn plan_remove_tag(
        &self,
        key: &CurationLifecycleKey,
        label: &str,
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        let labels = [label.to_owned()];
        self.plan_tag_operation(key, TagMutation::Remove, &labels, target_paths)
    }

    /// Plan one exact removal by the persisted source tag identifier.
    pub(super) fn plan_remove_tag_id(
        &self,
        key: &CurationLifecycleKey,
        tag_id: i64,
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        self.plan_tag_operation(
            key,
            TagMutation::RemoveExisting { tag_id },
            &[],
            target_paths,
        )
    }

    /// Plan one exact assignment by the persisted source tag identifier.
    pub(super) fn plan_add_tag_id(
        &self,
        key: &CurationLifecycleKey,
        tag_id: i64,
        target_paths: &[PathBuf],
    ) -> CurationPlan {
        self.plan_tag_operation(key, TagMutation::AddExisting { tag_id }, &[], target_paths)
    }

    /// Record an operation only after the catalog accepted its command.
    pub(super) fn accept_operation(
        &mut self,
        key: &CurationLifecycleKey,
        plan: CurationOperationPlan,
    ) -> bool {
        let next_request_id = self.next_request_id();
        let Some(lineage) = self.lineages.get_mut(key) else {
            return false;
        };
        if lineage.failed.is_some()
            || lineage.invalidated.is_some()
            || plan.root_identity != lineage.root_identity
        {
            return false;
        }
        if plan.request_id != next_request_id {
            return false;
        }
        self.next_request_id = plan.request_id;
        let tags_may_change = plan.tags_may_change();
        lineage.message = None;
        lineage.pending.push(PendingCurationOperation {
            request_id: plan.request_id,
            batch_count: plan.batches.len(),
            tags_may_change,
            overlays: plan.overlays,
            active_overlay: true,
            trash_required: plan.trash_required,
        });
        true
    }

    /// Consume one terminal catalog outcome, retaining receipts and reapplying newer overlays.
    fn apply_outcome(
        &mut self,
        key: CurationLifecycleKey,
        request_id: u64,
        outcome: CurationOutcome,
    ) -> Option<CurationOutcomeApplication> {
        self.apply_outcome_with_projection(key, request_id, outcome, true)
    }

    fn apply_outcome_with_projection(
        &mut self,
        key: CurationLifecycleKey,
        request_id: u64,
        outcome: CurationOutcome,
        projectable: bool,
    ) -> Option<CurationOutcomeApplication> {
        if !projectable
            || self
                .lineages
                .get(&key)
                .is_some_and(|lineage| lineage.invalidated.is_some())
        {
            return self.discard_non_projectable_outcome(key, request_id);
        }
        let Some(lineage) = self.lineages.get_mut(&key) else {
            self.retired_pending.remove(&(key, request_id));
            return None;
        };
        let Some(operation_index) = lineage
            .pending
            .iter()
            .position(|operation| operation.request_id == request_id)
        else {
            self.retired_pending.remove(&(key, request_id));
            return None;
        };
        let (
            batch_count,
            operation_tags_may_change,
            operation_trash_required,
            operation_active,
            operation_paths,
        ) = {
            let operation = &lineage.pending[operation_index];
            (
                operation.batch_count,
                operation.tags_may_change,
                operation.trash_required,
                operation.active_overlay,
                operation.overlays.keys().cloned().collect::<HashSet<_>>(),
            )
        };
        let expected_initial = outcome.initial_revision;
        let mut next_revision = lineage.revision;
        let prior_revision = lineage.revision;
        let mut valid =
            operation_active && expected_initial.is_none_or(|revision| revision == next_revision);
        let mut receipt_updates = Vec::new();
        let completed_batch_count = outcome.committed_batches.len();
        let mut affected = operation_paths;
        for receipt in &outcome.committed_batches {
            let expected_revision = next_revision.checked_add(1);
            if expected_revision != Some(receipt.source_revision)
                || receipt.root_identity != lineage.root_identity
                || receipt.directory_generation != lineage.directory_generation
                || !receipt.valid
                || receipt
                    .files
                    .keys()
                    .any(|path| !lineage.confirmed.contains_key(path))
                || receipt.playback_types.len() != receipt.files.len()
                || receipt
                    .files
                    .keys()
                    .any(|path| !receipt.playback_types.contains_key(path))
            {
                valid = false;
                break;
            }
            next_revision = receipt.source_revision;
            affected.extend(receipt.files.keys().cloned());
            receipt_updates.push(receipt);
        }
        if outcome.failure.is_none()
            && (completed_batch_count != batch_count
                || outcome.final_revision != Some(next_revision))
        {
            valid = false;
        }
        if outcome.failure.is_some()
            && outcome.final_revision.is_some()
            && outcome.final_revision != Some(next_revision)
        {
            valid = false;
        }
        if !valid {
            receipt_updates.clear();
            next_revision = lineage.revision;
        }
        let mut playback_types = BTreeMap::new();
        for receipt in receipt_updates {
            for (path, curation) in &receipt.files {
                lineage.confirmed.insert(path.clone(), *curation);
            }
            playback_types.extend(
                receipt
                    .playback_types
                    .iter()
                    .map(|(path, playback_type)| (path.clone(), *playback_type)),
            );
        }
        lineage.revision = next_revision;
        let failed = if !operation_active {
            Some(String::from(
                "dependent curation operation was invalidated after an earlier failure",
            ))
        } else if valid {
            outcome.failure
        } else {
            Some(String::from(
                "catalog returned a stale or incoherent curation receipt",
            ))
        };
        let success = failed.is_none();
        lineage.pending.remove(operation_index);
        if let Some(error) = failed.as_ref() {
            lineage.failed = Some(error.clone());
            for operation in &mut lineage.pending {
                operation.active_overlay = false;
                affected.extend(operation.overlays.keys().cloned());
            }
        }
        let visible = affected
            .into_iter()
            .filter_map(|path| effective_curation(lineage, &path).map(|curation| (path, curation)))
            .collect::<BTreeMap<_, _>>();
        let metadata_changed = !visible.is_empty() || !playback_types.is_empty();
        if success {
            lineage.message = Some(if operation_trash_required == 0 {
                format!("Curation saved at revision {}", lineage.revision)
            } else {
                format!(
                    "Curation saved at revision {}; trash unavailable for {} file{}",
                    lineage.revision,
                    operation_trash_required,
                    if operation_trash_required == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            });
        }
        Some(CurationOutcomeApplication {
            root_identity: lineage.root_identity.clone(),
            directory_generation: lineage.directory_generation,
            prior_revision,
            revision: lineage.revision,
            visible,
            playback_types,
            metadata_changed,
            tags_may_change: operation_tags_may_change,
            failed: failed.is_some(),
            projectable: true,
        })
    }

    fn discard_non_projectable_outcome(
        &mut self,
        key: CurationLifecycleKey,
        request_id: u64,
    ) -> Option<CurationOutcomeApplication> {
        let Some(lineage) = self.lineages.get_mut(&key) else {
            self.retired_pending.remove(&(key, request_id));
            return None;
        };
        let Some(operation_index) = lineage
            .pending
            .iter()
            .position(|operation| operation.request_id == request_id)
        else {
            self.retired_pending.remove(&(key, request_id));
            return None;
        };
        let revision = lineage.revision;
        lineage.pending.remove(operation_index);
        for operation in &mut lineage.pending {
            operation.active_overlay = false;
        }
        lineage.invalidated = Some(String::from(
            "the source publication fence became stale after the durable commit",
        ));
        lineage.message = None;
        Some(CurationOutcomeApplication {
            root_identity: lineage.root_identity.clone(),
            directory_generation: lineage.directory_generation,
            prior_revision: revision,
            revision,
            visible: BTreeMap::new(),
            playback_types: BTreeMap::new(),
            metadata_changed: false,
            tags_may_change: false,
            failed: false,
            projectable: false,
        })
    }

    fn next_request_id(&self) -> u64 {
        let next = self.next_request_id.wrapping_add(1);
        if next == 0 { 1 } else { next }
    }
}

impl CurationOutcome {
    fn from_catalog(outcome: &SourceCurationOperationOutcome) -> Self {
        Self {
            initial_revision: outcome.initial_revision(),
            final_revision: outcome.final_revision(),
            committed_batches: outcome
                .committed_batches()
                .iter()
                .map(CurationReceipt::from_catalog)
                .collect(),
            failure: outcome
                .failure()
                .map(|failure| format!("batch {}: {}", failure.batch_index(), failure.error())),
        }
    }
}

impl CurationReceipt {
    fn from_catalog(receipt: &SourceCurationCommitReceipt) -> Self {
        let mut files = BTreeMap::new();
        let mut playback_types = BTreeMap::new();
        let mut valid = true;
        for metadata in receipt.files() {
            let Some(curation) = curation_from_metadata(metadata) else {
                valid = false;
                continue;
            };
            let path = metadata.relative_path.clone();
            if files.insert(path.clone(), curation).is_some()
                || playback_types
                    .insert(path, playback_type_from_metadata(metadata))
                    .is_some()
            {
                valid = false;
            }
        }
        Self {
            source_revision: receipt.source_revision(),
            root_identity: receipt.root_identity().clone(),
            directory_generation: receipt.directory_generation(),
            files,
            playback_types,
            valid,
        }
    }
}

fn effective_curation(lineage: &CurationLineage, path: &PathBuf) -> Option<AudioFileCuration> {
    lineage
        .pending
        .iter()
        .rev()
        .filter(|operation| operation.active_overlay)
        .find_map(|operation| operation.overlays.get(path).copied())
        .or_else(|| lineage.confirmed.get(path).copied())
}

fn source_update_may_change_tags(update: &SourceCurationUpdate) -> bool {
    !matches!(
        update,
        SourceCurationUpdate::SetLastPlayedAt { .. }
            | SourceCurationUpdate::SetRating { .. }
            | SourceCurationUpdate::SetLocked { .. }
            | SourceCurationUpdate::AddCollection { .. }
            | SourceCurationUpdate::RemoveCollection { .. }
    )
}

fn updates_for_action(
    current: AudioFileCuration,
    after: AudioFileCuration,
    action: CurationAction,
    path: RootRelativePath,
) -> Vec<SourceCurationUpdate> {
    match action {
        CurationAction::AdjustRating { .. } => {
            let mut updates = Vec::with_capacity(2);
            if current.rating().value() != after.rating().value()
                || current.rating().locked() != after.rating().locked()
            {
                updates.push(SourceCurationUpdate::SetRating {
                    path: path.clone(),
                    rating: Rating::new(after.rating().value()),
                });
            }
            if current.rating().locked() != after.rating().locked() {
                updates.push(SourceCurationUpdate::SetLocked {
                    path,
                    locked: after.rating().locked(),
                });
            }
            updates
        }
        CurationAction::ToggleCollection { slot } => {
            let Some(collection) = SampleCollection::new(slot as u8) else {
                return Vec::new();
            };
            if after.has_collection(slot) {
                vec![SourceCurationUpdate::AddCollection { path, collection }]
            } else {
                vec![SourceCurationUpdate::RemoveCollection { path, collection }]
            }
        }
        CurationAction::RemoveCollection { slot } => {
            let Some(collection) = SampleCollection::new(slot as u8) else {
                return Vec::new();
            };
            (current.has_collection(slot) && !after.has_collection(slot))
                .then_some(SourceCurationUpdate::RemoveCollection { path, collection })
                .into_iter()
                .collect()
        }
        CurationAction::RecordPlayback { played_at } => {
            vec![SourceCurationUpdate::SetLastPlayedAt {
                path,
                played_at: Some(played_at),
            }]
        }
        CurationAction::UnlockKeepRating => (current.rating().locked() != after.rating().locked())
            .then_some(SourceCurationUpdate::SetLocked {
                path,
                locked: after.rating().locked(),
            })
            .into_iter()
            .collect(),
    }
}

fn path_byte_len(path: &Path) -> usize {
    #[cfg(unix)]
    {
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().len()
    }
}

fn consider_tag_target_path<'a>(
    path: &'a Path,
    label_count: usize,
    max_distinct_paths: usize,
    seen_paths: &mut HashSet<&'a Path>,
    paths: &mut Vec<PathBuf>,
    path_bytes: &mut usize,
) -> Result<(), String> {
    if !seen_paths.insert(path) {
        return Ok(());
    }
    if paths.len() >= max_distinct_paths {
        return Err(format!(
            "Tag selection needs more than {} updates",
            MAX_CURATION_OPERATION_UPDATES
        ));
    }
    let path_bytes_for_row = path_byte_len(path)
        .checked_mul(label_count)
        .ok_or_else(|| {
            String::from("Tag selection path payload exceeds the curation byte bound")
        })?;
    *path_bytes = path_bytes.checked_add(path_bytes_for_row).ok_or_else(|| {
        String::from("Tag selection path payload exceeds the curation byte bound")
    })?;
    if *path_bytes > MAX_CURATION_OPERATION_PATH_BYTES {
        return Err(format!(
            "Selection paths exceed the {} byte curation bound",
            MAX_CURATION_OPERATION_PATH_BYTES
        ));
    }
    paths.push(path.to_path_buf());
    Ok(())
}

fn curation_from_metadata(metadata: &BrowserFileMetadata) -> Option<AudioFileCuration> {
    let collection_mask = metadata.collections.iter().fold(0_u8, |mask, collection| {
        let slot = usize::from(collection.index());
        (slot < COLLECTION_COUNT)
            .then(|| mask | (1 << slot))
            .unwrap_or(mask)
    });
    AudioFileCuration::from_parts(
        metadata.rating.val(),
        metadata.locked,
        collection_mask,
        metadata.last_played_at,
        metadata.last_curated_at,
    )
}

fn playback_type_from_metadata(metadata: &BrowserFileMetadata) -> Option<PlaybackType> {
    match metadata.playback_type {
        Some(SourcePlaybackType::OneShot) => Some(PlaybackType::OneShot),
        Some(SourcePlaybackType::Loop) => Some(PlaybackType::Loop),
        None => None,
    }
}

/// Render the six fixed collection filters for the active source.
pub(super) fn render_collection_filters(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let active_source = browser.active_source.is_some();
    let active_filter = browser.view_model.collection_filter();
    let counts = browser.collection_filter_counts;
    let rows = (0..COLLECTION_COUNT)
        .map(|slot| {
            let selected = active_filter == Some(slot);
            let slot_color = color(theme.colors.collection_slots[slot]);
            let hover_fill = color(theme.colors.selection_hover_fill);
            let pressed_fill = color(theme.interaction.pressed_fill);
            let label = format!("Collection {}", slot + 1);
            div()
                .id(("collection-filter", slot))
                .when_some(
                    browser.ax_focus(&super::accessibility::collection_filter_id(slot)),
                    |element, focus| element.track_focus(&focus),
                )
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
                .when(!active_source, |element| element.opacity(0.45))
                .when_some(
                    browser.ax_marker(super::accessibility::collection_filter_id(slot)),
                    |element, marker| element.child(marker),
                )
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if active_source {
                        view.toggle_collection_filter(slot, cx);
                    }
                }))
                .child(
                    div()
                        .flex_none()
                        .ml(px(theme.spacing.row_padding_x))
                        .mr(px(theme.spacing.control_gap))
                        .w(px(8.0))
                        .h(px(8.0))
                        .bg(slot_color),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(theme.typography.small_size))
                        .text_color(if selected {
                            color(theme.colors.text_primary)
                        } else {
                            color(theme.colors.text_muted)
                        })
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .mr(px(theme.spacing.row_padding_x))
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(theme.colors.text_disabled))
                        .child(counts[slot].to_string()),
                )
        })
        .collect::<Vec<_>>();
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
                .flex_none()
                .items_center()
                .h(px(theme.spacing.row_height))
                .px(px(theme.spacing.panel_padding))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child("SOURCE COLLECTIONS"),
        )
        .children(rows)
}

/// Render the two fixed playback-type filters for the active source.
pub(super) fn render_playback_type_filters(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let active_source = browser.active_source.is_some();
    let active_filter = browser.view_model.playback_type_filter();
    let hover_fill = color(theme.colors.selection_hover_fill);
    let pressed_fill = color(theme.interaction.pressed_fill);
    let chip_width = compact_filter_chip_width(&theme, PLAYBACK_TYPE_FILTERS.len());
    let chips = PLAYBACK_TYPE_FILTERS
        .into_iter()
        .enumerate()
        .map(|(index, playback_type)| {
            let selected = active_filter.contains_type(playback_type);
            let label = playback_type_filter_label(playback_type);
            div()
                .id(("playback-type-filter", index))
                .when_some(
                    browser.ax_focus(&super::accessibility::playback_type_filter_id(index)),
                    |element, focus| element.track_focus(&focus),
                )
                .flex()
                .flex_none()
                .w(px(chip_width))
                .h(px(theme.spacing.row_height))
                .items_center()
                .justify_center()
                .cursor_pointer()
                .rounded(px(3.0))
                .bg(if selected {
                    color(theme.colors.selection_fill)
                } else {
                    transparent()
                })
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed_fill))
                .when(!active_source, |element| element.opacity(0.45))
                .when_some(
                    browser.ax_marker(super::accessibility::playback_type_filter_id(index)),
                    |element, marker| element.child(marker),
                )
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if active_source {
                        view.toggle_playback_type_filter(playback_type, cx);
                    }
                }))
                .child(
                    div()
                        .text_size(px(theme.typography.small_size))
                        .text_color(if selected {
                            color(theme.colors.text_primary)
                        } else {
                            color(theme.colors.text_muted)
                        })
                        .child(label),
                )
        })
        .collect::<Vec<_>>();
    compact_filter_row(&theme, "TYPE", chips)
}

fn playback_type_filter_label(playback_type: PlaybackType) -> &'static str {
    match playback_type {
        PlaybackType::OneShot => "1-SHOT",
        PlaybackType::Loop => "LOOP",
    }
}

/// Render the eight fixed rating filters for the active source.
pub(super) fn render_rating_filters(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let active_source = browser.active_source.is_some();
    let active_filter = browser.view_model.rating_filter();
    let hover_fill = color(theme.colors.selection_hover_fill);
    let pressed_fill = color(theme.interaction.pressed_fill);
    let chip_width = compact_filter_chip_width(&theme, RATING_FILTER_LEVELS.len());
    let chips = RATING_FILTER_LEVELS
        .into_iter()
        .enumerate()
        .map(|(index, level)| {
            let selected = active_filter.contains_level(level);
            let marker_color = color(rating_filter_color(level, &theme));
            div()
                .id(("rating-filter", index))
                .when_some(
                    browser.ax_focus(&super::accessibility::rating_filter_id(index)),
                    |element, focus| element.track_focus(&focus),
                )
                .flex()
                .flex_none()
                .w(px(chip_width))
                .h(px(theme.spacing.row_height))
                .items_center()
                .justify_center()
                .gap(px(2.0))
                .cursor_pointer()
                .rounded(px(3.0))
                .bg(if selected {
                    color(theme.colors.selection_fill)
                } else {
                    transparent()
                })
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed_fill))
                .when(!active_source, |element| element.opacity(0.45))
                .when_some(
                    browser.ax_marker(super::accessibility::rating_filter_id(index)),
                    |element, marker| element.child(marker),
                )
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if active_source {
                        view.toggle_rating_filter(level, cx);
                    }
                }))
                .child(div().flex_none().w(px(10.0)).h(px(10.0)).bg(marker_color))
        })
        .collect::<Vec<_>>();
    compact_filter_row(&theme, "RATING", chips)
}

fn rating_filter_color(level: i8, theme: &Theme) -> yield_design::Color {
    if level < 0 {
        theme.colors.rating_trash
    } else if level > 0 {
        theme.colors.rating_keep
    } else {
        theme.colors.rating_neutral
    }
}

/// Return the compact rating cell representation for a row.
pub(super) fn rating_cell(entry: &AudioFile, theme: &Theme, width: f32) -> impl gpui::IntoElement {
    let Some(curation) = entry.curation else {
        return div()
            .flex()
            .items_center()
            .h_full()
            .flex_none()
            .w(px(width))
            .px(px(theme.spacing.control_gap))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_disabled))
            .child("—");
    };
    let rating = curation.rating();
    let magnitude = usize::from(rating.value().unsigned_abs());
    let marker_color = if rating.value() < 0 {
        theme.colors.rating_trash
    } else if rating.value() > 0 {
        theme.colors.rating_keep
    } else {
        theme.colors.rating_neutral
    };
    let show_lock = rating.locked() && rating.value() > 0;
    let marker_count = magnitude + usize::from(show_lock);
    let content_width = magnitude as f32 * 5.0
        + if show_lock { 9.0 } else { 0.0 }
        + marker_count.saturating_sub(1) as f32 * 2.0;
    let padding = theme
        .spacing
        .control_gap
        .min(((width - content_width) / 2.0).max(0.0));
    let mut markers = Vec::with_capacity(marker_count);
    for _ in 0..magnitude {
        markers.push(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded_full()
                .bg(color(marker_color)),
        );
    }
    if show_lock {
        markers.push(
            div()
                .flex_none()
                .ml(px(2.0))
                .w(px(7.0))
                .h(px(7.0))
                .border(px(theme.spacing.border_width.min(3.5)))
                .border_color(color(theme.colors.rating_keep)),
        );
    }
    div()
        .flex()
        .flex_none()
        .w(px(width))
        .items_center()
        .h_full()
        .gap(px(2.0))
        .px(px(padding))
        .children(markers)
}

/// Return the compact collection swatches for a row.
pub(super) fn collection_cell(
    entry: &AudioFile,
    theme: &Theme,
    width: f32,
) -> impl gpui::IntoElement {
    const MIN_SWATCH_SIZE: f32 = 2.0;
    let width = width.max(0.0);
    let max_padding = ((width - MIN_SWATCH_SIZE * COLLECTION_COUNT as f32) / 2.0).max(0.0);
    let padding = theme.spacing.control_gap.min(max_padding);
    let available = (width - padding * 2.0).max(0.0);
    let swatch_size = (available / COLLECTION_COUNT as f32).clamp(MIN_SWATCH_SIZE, 6.0);
    let gap = if swatch_size >= 6.0 {
        ((available - swatch_size * COLLECTION_COUNT as f32) / (COLLECTION_COUNT - 1) as f32)
            .clamp(0.0, 3.0)
    } else {
        0.0
    };
    let mut swatches = Vec::new();
    if let Some(curation) = entry.curation {
        for slot in 0..COLLECTION_COUNT {
            if curation.has_collection(slot) {
                swatches.push(
                    div()
                        .flex_none()
                        .w(px(swatch_size))
                        .h(px(swatch_size))
                        .bg(color(theme.colors.collection_slots[slot])),
                );
            }
        }
    }
    div()
        .flex()
        .flex_none()
        .w(px(width))
        .items_center()
        .h_full()
        .gap(px(gap))
        .px(px(padding))
        .children(swatches)
}

impl Browser {
    pub(super) fn refresh_collection_filter_counts(&mut self) {
        self.collection_filter_counts = self.view_model.collection_counts(&self.entries);
    }

    pub(super) fn reset_collection_filter(&mut self) {
        self.view_model.set_collection_filter(&self.entries, None);
        self.refresh_collection_filter_counts();
    }

    pub(super) fn toggle_rating_filter(&mut self, level: i8, cx: &mut Context<Self>) {
        if self.active_source.is_none() {
            return;
        }
        self.invalidate_file_handoff();
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.reset_browser_focus_state();
        self.context_menu = None;
        if !self.view_model.toggle_rating_filter(&self.entries, level) {
            return;
        }
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }

    pub(super) fn toggle_playback_type_filter(
        &mut self,
        playback_type: PlaybackType,
        cx: &mut Context<Self>,
    ) {
        if self.active_source.is_none() {
            return;
        }
        self.invalidate_file_handoff();
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.context_menu = None;
        if !self
            .view_model
            .toggle_playback_type_filter(&self.entries, playback_type)
        {
            return;
        }
        self.reset_browser_focus_state();
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }

    pub(super) fn toggle_collection_filter(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.active_source.is_none() {
            return;
        }
        self.invalidate_file_handoff();
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.reset_browser_focus_state();
        self.context_menu = None;
        if !self
            .view_model
            .toggle_collection_filter(&self.entries, slot)
        {
            return;
        }
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }

    /// Route one plain browser curation shortcut to the catalog-owned operation planner.
    pub(super) fn handle_curation_shortcut(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(action) = shortcut_action(key) else {
            return false;
        };
        self.submit_curation_action(action, None, cx);
        true
    }

    /// Record an accepted playback against the exact active-source member that was played.
    pub(super) fn record_playback_for_intent(
        &mut self,
        absolute_path: &Path,
        played_at: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(relative_path) = self.playback_target_relative_path(absolute_path) else {
            self.scan_status =
                String::from("Playback history not saved: sample metadata is unavailable");
            cx.notify();
            return;
        };
        self.submit_curation_action(
            CurationAction::RecordPlayback { played_at },
            Some(vec![relative_path]),
            cx,
        );
    }

    pub(super) fn submit_curation_action(
        &mut self,
        action: CurationAction,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        let Some(activation) = self.catalog_activation.as_ref() else {
            self.scan_status = if is_playback_record_action(action) {
                String::from("Playback history unavailable: sample metadata is not ready")
            } else {
                String::from("Curation requires a successful audited catalog projection")
            };
            cx.notify();
            return;
        };
        if !activation.audit_finished || activation.audit_error.is_some() {
            self.scan_status = if is_playback_record_action(action) {
                String::from("Playback history unavailable: sample metadata is not ready")
            } else {
                String::from("Curation requires a successful audited catalog projection")
            };
            cx.notify();
            return;
        }
        let Some(context) = self.catalog_metadata.context() else {
            self.scan_status = if is_playback_record_action(action) {
                String::from("Playback history unavailable: sample metadata is not ready")
            } else {
                String::from(
                    "Curation metadata is still loading; try again after the catalog is ready",
                )
            };
            cx.notify();
            return;
        };
        let key = CurationLifecycleKey {
            source_id: activation.source_id.clone(),
            lifecycle_generation: activation.lifecycle_generation,
        };
        if context.source_id != key.source_id
            || context.lifecycle_generation != key.lifecycle_generation
            || context.root != activation.root
            || !self.curation_state.authority_matches(
                &key,
                &context.root_identity,
                context.directory_generation,
                context.revision,
            )
        {
            self.curation_state.set_message(
                &key,
                if is_playback_record_action(action) {
                    String::from("Playback history unavailable: sample metadata is stale")
                } else {
                    String::from("Curation authority is stale; refresh the catalog before editing")
                },
            );
            cx.notify();
            return;
        }
        if tag_filter::scope_pending(self) && !is_playback_record_action(action) {
            self.curation_state.set_message(
                &key,
                String::from("Tag filtering is still loading; wait for the visible scope"),
            );
            cx.notify();
            return;
        }
        let target_paths = explicit_paths.unwrap_or_else(|| self.curation_target_paths());
        match self.curation_state.plan_action(&key, action, &target_paths) {
            CurationPlan::Rejected(message) => {
                self.curation_state.set_message(&key, message);
            }
            CurationPlan::NoChange { trash_required } => {
                if trash_required == 0 && is_playback_record_action(action) {
                    return;
                }
                let message = if trash_required == 0 {
                    String::from("No curation changes")
                } else {
                    format!(
                        "Trash unavailable for {trash_required} file{}; no metadata changed",
                        if trash_required == 1 { "" } else { "s" }
                    )
                };
                self.curation_state.set_message(&key, message);
            }
            CurationPlan::Ready(plan) => {
                let request_id = plan.request_id;
                let tags_may_change = plan.tags_may_change();
                let reusable_tag_mask = (!tags_may_change)
                    .then(|| tag_filter::capture_ready_mask(self))
                    .flatten();
                let optimistic = plan.overlays.clone();
                let command = yield_session::CatalogCommand::ApplyCurationOperation {
                    source_id: key.source_id.clone(),
                    lifecycle_generation: key.lifecycle_generation,
                    fence: plan.fence,
                    root_identity: plan.root_identity.clone(),
                    request_id,
                    batches: plan.batches.clone(),
                };
                match catalog::submit_catalog_command(command, cx) {
                    Ok(()) => {
                        if !self.curation_state.accept_operation(&key, plan) {
                            self.curation_state.set_message(
                                &key,
                                if is_playback_record_action(action) {
                                    String::from("Playback history unavailable: metadata changed")
                                } else {
                                    String::from(
                                        "Curation admission changed; refresh the catalog before editing",
                                    )
                                },
                            );
                        } else {
                            self.catalog_metadata.pause_for_curation();
                            self.display_labels.pause_for_curation(tags_may_change);
                            tag_chips::invalidate_after_curation(self);
                            if tags_may_change
                                || (reusable_tag_mask.is_none()
                                    && self.view_model.tag_filter().is_enabled())
                            {
                                tag_filter::invalidate_after_curation(
                                    self,
                                    tag_filter::FilterOrigin::PassiveMetadata,
                                    cx,
                                );
                            }
                            self.apply_visible_curation(
                                &key,
                                &optimistic,
                                &BTreeMap::new(),
                                reusable_tag_mask,
                                cx,
                            );
                            self.start_source_registry_poll(cx);
                        }
                    }
                    Err(error) => {
                        let label = if is_playback_record_action(action) {
                            "Playback history"
                        } else {
                            "Curation"
                        };
                        self.curation_state
                            .set_message(&key, format!("{label} not saved: {error}"));
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn submit_add_tags(
        &mut self,
        labels: Vec<String>,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) -> TagAdmissionResult {
        self.submit_tag_operation(TagMutation::Add, labels, explicit_paths, cx)
    }

    /// Submit one bounded assignment for a persisted source-tag identifier.
    pub(super) fn submit_add_tag_id(
        &mut self,
        tag_id: i64,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) -> TagAdmissionResult {
        self.submit_tag_operation(
            TagMutation::AddExisting { tag_id },
            Vec::new(),
            explicit_paths,
            cx,
        )
    }

    #[expect(
        dead_code,
        reason = "tag-chip removal will use the typed admission result later"
    )]
    pub(super) fn submit_remove_tag(
        &mut self,
        label: String,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) -> TagAdmissionResult {
        self.submit_tag_operation(TagMutation::Remove, vec![label], explicit_paths, cx)
    }

    /// Submit one bounded removal for a persisted source-tag identifier.
    pub(super) fn submit_remove_tag_id(
        &mut self,
        tag_id: i64,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) -> TagAdmissionResult {
        self.submit_tag_operation(
            TagMutation::RemoveExisting { tag_id },
            Vec::new(),
            explicit_paths,
            cx,
        )
    }

    fn submit_tag_operation(
        &mut self,
        mutation: TagMutation,
        labels: Vec<String>,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) -> TagAdmissionResult {
        let Some(activation) = self.catalog_activation.as_ref() else {
            self.scan_status =
                String::from("Tag editing requires a successful audited catalog projection");
            cx.notify();
            return TagAdmissionResult::Rejected(self.scan_status.clone());
        };
        if !activation.audit_finished || activation.audit_error.is_some() {
            self.scan_status =
                String::from("Tag editing requires a successful audited catalog projection");
            cx.notify();
            return TagAdmissionResult::Rejected(self.scan_status.clone());
        }
        let Some(context) = self.catalog_metadata.context() else {
            self.scan_status = String::from(
                "Curation metadata is still loading; try again after the catalog is ready",
            );
            cx.notify();
            return TagAdmissionResult::Rejected(self.scan_status.clone());
        };
        let key = CurationLifecycleKey {
            source_id: activation.source_id.clone(),
            lifecycle_generation: activation.lifecycle_generation,
        };
        if context.source_id != key.source_id
            || context.lifecycle_generation != key.lifecycle_generation
            || context.root != activation.root
            || !self.curation_state.authority_matches(
                &key,
                &context.root_identity,
                context.directory_generation,
                context.revision,
            )
        {
            self.curation_state.set_message(
                &key,
                String::from("Curation authority is stale; refresh the catalog before editing"),
            );
            cx.notify();
            return TagAdmissionResult::Rejected(String::from(
                "Curation authority is stale; refresh the catalog before editing",
            ));
        }
        if tag_filter::scope_pending(self) {
            let message =
                String::from("Tag filtering is still loading; wait for the visible scope");
            self.curation_state.set_message(&key, message.clone());
            cx.notify();
            return TagAdmissionResult::Rejected(message);
        }
        let target_paths = explicit_paths.unwrap_or_else(|| self.curation_target_paths());
        let plan = match mutation {
            TagMutation::Add => self
                .curation_state
                .plan_add_tags(&key, &labels, &target_paths),
            TagMutation::AddExisting { tag_id } => {
                self.curation_state
                    .plan_add_tag_id(&key, tag_id, &target_paths)
            }
            TagMutation::Remove => self.curation_state.plan_remove_tag(
                &key,
                labels.first().map(String::as_str).unwrap_or_default(),
                &target_paths,
            ),
            TagMutation::RemoveExisting { tag_id } => {
                self.curation_state
                    .plan_remove_tag_id(&key, tag_id, &target_paths)
            }
        };
        let result = match plan {
            CurationPlan::Rejected(message) => {
                self.curation_state.set_message(&key, message.clone());
                TagAdmissionResult::Rejected(message)
            }
            CurationPlan::NoChange { .. } => {
                self.curation_state
                    .set_message(&key, String::from("No tag changes"));
                TagAdmissionResult::NoChange
            }
            CurationPlan::Ready(plan) => {
                let request_id = plan.request_id;
                let command = yield_session::CatalogCommand::ApplyCurationOperation {
                    source_id: key.source_id.clone(),
                    lifecycle_generation: key.lifecycle_generation,
                    fence: plan.fence,
                    root_identity: plan.root_identity.clone(),
                    request_id,
                    batches: plan.batches.clone(),
                };
                match catalog::submit_catalog_command(command, cx) {
                    Ok(()) => {
                        if !self.curation_state.accept_operation(&key, plan) {
                            self.curation_state.set_message(
                                &key,
                                String::from(
                                    "Curation admission changed; refresh the catalog before editing",
                                ),
                            );
                            TagAdmissionResult::Rejected(String::from(
                                "Curation admission changed; refresh the catalog before editing",
                            ))
                        } else {
                            // Tag writes have no optimistic tag vector. The committed receipt is
                            // the first source of visible tag and playback-type state.
                            self.catalog_metadata.pause_for_curation();
                            self.display_labels.pause_for_curation(true);
                            tag_chips::invalidate_after_curation(self);
                            tag_filter::invalidate_after_curation(
                                self,
                                tag_filter::FilterOrigin::PassiveMetadata,
                                cx,
                            );
                            self.start_source_registry_poll(cx);
                            TagAdmissionResult::Accepted
                        }
                    }
                    Err(error) => {
                        let message = format!("Tags not saved: {error}");
                        self.curation_state.set_message(&key, message.clone());
                        TagAdmissionResult::Rejected(message)
                    }
                }
            }
        };
        cx.notify();
        result
    }

    /// Capture the current tag targets while checking the curation bounds before cloning paths.
    pub(super) fn capture_tag_target_paths(
        &self,
        label_count: usize,
    ) -> Result<Vec<PathBuf>, String> {
        if tag_filter::scope_pending(self) {
            return Err(String::from(
                "Tag filtering is still loading; wait for the visible scope",
            ));
        }
        if label_count == 0 {
            return Err(String::from("Tag operation requires at least one label"));
        }
        let max_distinct_paths = MAX_CURATION_OPERATION_UPDATES / label_count;
        if max_distinct_paths == 0 {
            return Err(String::from(
                "Tag selection exceeds the curation update bound",
            ));
        }
        let seen_path_capacity = self.selection.len().min(max_distinct_paths).max(1);
        let mut seen_paths = HashSet::with_capacity(seen_path_capacity);
        let mut paths = Vec::new();
        let mut path_bytes = 0usize;

        if !self.selection.is_empty() {
            for row in self.view_model.rows() {
                if !self.selection.contains(&row.id) {
                    continue;
                }
                let Some(entry) = self.entries.get(row.entry_index) else {
                    continue;
                };
                consider_tag_target_path(
                    &entry.relative_path,
                    label_count,
                    max_distinct_paths,
                    &mut seen_paths,
                    &mut paths,
                    &mut path_bytes,
                )?;
            }
        } else if let Some(index) = self.browser_focus_index()
            && let Some(entry) = self.entries.get(index)
        {
            consider_tag_target_path(
                &entry.relative_path,
                label_count,
                max_distinct_paths,
                &mut seen_paths,
                &mut paths,
                &mut path_bytes,
            )?;
        }
        Ok(paths)
    }

    fn curation_target_paths(&self) -> Vec<PathBuf> {
        if tag_filter::scope_pending(self) {
            return Vec::new();
        }
        let visible_rows = self.view_model.rows();
        if !self.selection.is_empty() {
            return visible_rows
                .iter()
                .filter(|row| self.selection.contains(&row.id))
                .filter_map(|row| self.entries.get(row.entry_index))
                .map(|entry| entry.relative_path.clone())
                .collect();
        }
        self.browser_focus_index()
            .and_then(|index| self.entries.get(index))
            .map(|entry| vec![entry.relative_path.clone()])
            .unwrap_or_default()
    }

    fn playback_target_relative_path(&self, absolute_path: &Path) -> Option<PathBuf> {
        let activation = self.catalog_activation.as_ref()?;
        if !activation.audit_finished || activation.audit_error.is_some() {
            return None;
        }
        let context = self.catalog_metadata.context()?;
        if context.source_id != activation.source_id
            || context.lifecycle_generation != activation.lifecycle_generation
            || context.root != activation.root
        {
            return None;
        }
        let active_index = self.active_source?;
        let session = self.source_sessions.get(active_index)?;
        if session.source_id.as_ref() != Some(&activation.source_id)
            || session.root != activation.root
            || session.catalog_lifecycle_generation != Some(activation.lifecycle_generation)
        {
            return None;
        }
        let relative_path = explicit_playback_target_path(
            &activation.root,
            &context.entry_indices,
            &self.entries,
            absolute_path,
        )?;
        let entry_index = context.entry_indices.get(&relative_path).copied()?;
        let session_entry = session.entries.get(entry_index)?;
        (session_entry.path == absolute_path && session_entry.relative_path == relative_path)
            .then_some(relative_path)
    }

    fn apply_visible_curation(
        &mut self,
        key: &CurationLifecycleKey,
        patches: &BTreeMap<PathBuf, AudioFileCuration>,
        playback_types: &BTreeMap<PathBuf, Option<PlaybackType>>,
        reusable_tag_mask: Option<TagFilterMask>,
        cx: &mut Context<Self>,
    ) {
        let Some(active_index) = self.active_source.filter(|index| {
            self.source_sessions.get(*index).is_some_and(|session| {
                session.source_id.as_ref() == Some(&key.source_id)
                    && session.catalog_lifecycle_generation == Some(key.lifecycle_generation)
            })
        }) else {
            return;
        };
        let (previous_id, previous_position) = tag_filter::projection_focus(self);
        let collection_filter = self.view_model.collection_filter();
        let rating_filter = self.view_model.rating_filter();
        let playback_type_filter = self.view_model.playback_type_filter();
        let sort_column = self.view_model.sort().column;
        let entry_indices = self
            .catalog_metadata
            .context()
            .filter(|context| {
                context.source_id == key.source_id
                    && context.lifecycle_generation == key.lifecycle_generation
            })
            .map(|context| context.entry_indices);
        let (
            collection_changed,
            filter_membership_changed,
            rating_filter_membership_changed,
            last_played_changed,
        ) = patch_visible_entries(
            &mut self.entries,
            patches,
            collection_filter,
            rating_filter,
            entry_indices.as_deref(),
        );
        patch_visible_entries(
            &mut self.source_sessions[active_index].entries,
            patches,
            collection_filter,
            rating_filter,
            entry_indices.as_deref(),
        );
        let (playback_type_changed, playback_type_filter_membership_changed) =
            patch_visible_playback_types(
                &mut self.entries,
                playback_types,
                playback_type_filter,
                entry_indices.as_deref(),
            );
        let preserve_passive_focus = playback_type_changed || self.browser_focus_detached;
        patch_visible_playback_types(
            &mut self.source_sessions[active_index].entries,
            playback_types,
            playback_type_filter,
            entry_indices.as_deref(),
        );
        self.reconcile_prepared_playback_type(playback_types);
        if collection_changed
            || rating_filter_membership_changed
            || playback_type_changed
            || playback_type_filter_membership_changed
        {
            self.refresh_collection_filter_counts();
        }
        if sort_column == yield_core::SortColumn::Rating
            || sort_column == yield_core::SortColumn::Collection
            || (sort_column == yield_core::SortColumn::LastPlayed && last_played_changed)
            || (sort_column == yield_core::SortColumn::PlaybackType && playback_type_changed)
            || filter_membership_changed
            || rating_filter_membership_changed
            || playback_type_filter_membership_changed
        {
            self.view_model.set_entries(&self.entries);
            if let Some(mask) = reusable_tag_mask {
                if !tag_filter::apply_reusable_mask(self, mask) {
                    tag_filter::invalidate_after_curation(
                        self,
                        tag_filter::FilterOrigin::PassiveMetadata,
                        cx,
                    );
                    return;
                }
            }
            if tag_filter::scope_pending(self) {
                return;
            }
            self.reconcile_selection();
            if preserve_passive_focus {
                self.restore_playback_type_projection_focus(previous_id, previous_position, cx);
            } else {
                self.restore_metadata_projection_focus(previous_id, previous_position, cx);
            }
        }
    }

    pub(super) fn restore_metadata_projection_focus(
        &mut self,
        previous_id: Option<AudioFileId>,
        previous_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let focused_position = previous_id
            .as_ref()
            .and_then(|id| self.view_model.visible_position_for(id));
        if should_retain_passive_browser_focus(
            self.browser_focus_override.as_ref(),
            previous_id.as_ref(),
            focused_position.is_some(),
        ) {
            if let Some(position) = focused_position {
                self.list_scroll_handle
                    .scroll_to_item(position, ScrollStrategy::Center);
            }
            cx.notify();
            return;
        }
        self.reset_browser_focus_state();
        self.restore_projection_focus(previous_id, previous_position, cx);
    }

    pub(super) fn restore_playback_type_projection_focus(
        &mut self,
        previous_id: Option<AudioFileId>,
        previous_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let loaded_selection_detached = self
            .selected_id()
            .is_some_and(|id| self.view_model.visible_position_for(&id).is_none());
        match passive_projection_focus(&self.view_model, previous_id.as_ref(), previous_position) {
            PassiveProjectionFocus::Retain { visible_position } => {
                self.browser_focus_detached = loaded_selection_detached;
                self.list_scroll_handle
                    .scroll_to_item(visible_position, ScrollStrategy::Center);
            }
            PassiveProjectionFocus::Override {
                entry_index,
                visible_position,
            } => {
                let Some(entry) = self.entries.get(entry_index) else {
                    self.detach_browser_focus();
                    self.reset_focus();
                    cx.notify();
                    return;
                };
                self.set_browser_focus_override(entry.stable_id());
                self.browser_focus_detached = loaded_selection_detached;
                self.list_scroll_handle
                    .scroll_to_item(visible_position, ScrollStrategy::Center);
            }
            PassiveProjectionFocus::Detached => {
                self.detach_browser_focus();
                self.reset_focus();
            }
        }
        cx.notify();
    }

    fn reconcile_prepared_playback_type(
        &mut self,
        playback_types: &BTreeMap<PathBuf, Option<PlaybackType>>,
    ) {
        let Some(selected) = self
            .selected
            .and_then(|index| self.entries.get(index))
            .cloned()
        else {
            return;
        };
        let Some(playback_type) = playback_types.get(&selected.relative_path) else {
            return;
        };
        let file_id = selected.stable_id();
        if self.playback_policy.current_file() == Some(&file_id) {
            self.playback_policy.prepare(file_id, *playback_type);
        }
    }

    /// Consume a compound catalog outcome before active-source event filtering.
    pub(super) fn apply_curation_event(
        &mut self,
        event: &yield_session::CatalogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let yield_session::CatalogEvent::CurationOperationCompleted {
            source_id,
            lifecycle_generation,
            request_id,
            outcome,
            publication,
        } = event
        else {
            return false;
        };
        let reusable_candidate = self
            .catalog_activation
            .as_ref()
            .filter(|activation| {
                activation.source_id == *source_id
                    && activation.lifecycle_generation == *lifecycle_generation
            })
            .and_then(|_| tag_filter::capture_reusable_candidate(self));
        let key = CurationLifecycleKey {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        };
        let outcome = CurationOutcome::from_catalog(outcome);
        let projectable = *publication == CatalogPublicationStatus::Current
            || outcome.committed_batches.is_empty();
        let Some(application) = (if projectable {
            self.curation_state
                .apply_outcome(key.clone(), *request_id, outcome)
        } else {
            self.curation_state.apply_outcome_with_projection(
                key.clone(),
                *request_id,
                outcome,
                false,
            )
        }) else {
            // The projection may have been superseded, but the terminal event was still drained.
            return true;
        };
        let active_key = self
            .catalog_activation
            .as_ref()
            .map(|activation| CurationLifecycleKey {
                source_id: activation.source_id.clone(),
                lifecycle_generation: activation.lifecycle_generation,
            });
        self.curation_state
            .prune_completed_except(active_key.as_ref());
        let active = curation_key_is_active(active_key.as_ref(), &key);
        if !application.projectable {
            if active {
                self.invalidate_source_publication(
                    cx,
                    source_id,
                    *lifecycle_generation,
                    "Curation committed, but the source changed; refresh the catalog",
                );
            }
            return true;
        }
        if active {
            let tag_filter_revision_changed = !application.failed
                && self.catalog_metadata.context().is_some_and(|context| {
                    context.source_id == key.source_id
                        && context.lifecycle_generation == key.lifecycle_generation
                        && context.root_identity == application.root_identity
                        && context.directory_generation == application.directory_generation
                        && application.revision > context.revision
                });
            let reusable_tag_mask = if tag_filter_revision_changed {
                tag_filter::validate_reusable_candidate(
                    self,
                    reusable_candidate,
                    application.prior_revision,
                    application.revision,
                    &application.root_identity,
                    application.directory_generation,
                    application.tags_may_change,
                    !application.failed,
                )
            } else {
                None
            };
            let reused_tag_mask = reusable_tag_mask.is_some();
            if application.failed {
                let origin =
                    if !application.playback_types.is_empty() || self.browser_focus_detached {
                        tag_filter::FilterOrigin::PassivePlayback
                    } else {
                        tag_filter::FilterOrigin::PassiveMetadata
                    };
                tag_filter::invalidate_after_curation(self, origin, cx);
            } else if tag_filter_revision_changed && reusable_tag_mask.is_none() {
                let origin =
                    if !application.playback_types.is_empty() || self.browser_focus_detached {
                        tag_filter::FilterOrigin::PassivePlayback
                    } else {
                        tag_filter::FilterOrigin::PassiveMetadata
                    };
                tag_filter::invalidate_after_curation(self, origin, cx);
            }
            tag_chips::invalidate_after_curation(self);
            if application.metadata_changed {
                self.apply_visible_curation(
                    &key,
                    &application.visible,
                    &application.playback_types,
                    reusable_tag_mask,
                    cx,
                );
            }
            if self.curation_state.has_pending(&key) {
                if let Some(context) = self.catalog_metadata.context()
                    && context.source_id == key.source_id
                    && context.lifecycle_generation == key.lifecycle_generation
                    && context.root_identity == application.root_identity
                    && context.directory_generation == application.directory_generation
                    && !application.failed
                {
                    self.catalog_metadata
                        .advance_projection_revision(application.revision);
                }
                self.catalog_metadata.pause_for_curation();
            } else if !application.failed
                && let Some(context) = self.catalog_metadata.context()
                && context.source_id == key.source_id
                && context.lifecycle_generation == key.lifecycle_generation
                && context.root_identity == application.root_identity
                && context.directory_generation == application.directory_generation
            {
                self.catalog_metadata
                    .advance_projection_revision(application.revision);
            }
            let display_curation_pending = self.curation_state.has_pending_for_lifecycle(&key);
            let display_context = self
                .catalog_metadata
                .context()
                .map(super::display_labels::display_context_from_metadata);
            let display_labels_rebased = !application.failed
                && !application.tags_may_change
                && display_context.clone().is_some_and(|context| {
                    self.display_labels.rebase_after_curation(
                        context,
                        application.prior_revision,
                        application.revision,
                        &application.root_identity,
                        application.directory_generation,
                        display_curation_pending,
                    )
                });
            if !display_labels_rebased {
                self.display_labels
                    .invalidate_after_curation(display_context, display_curation_pending);
            }
            if reused_tag_mask {
                if !tag_filter::rebase_ready_context(
                    self,
                    application.revision,
                    &application.root_identity,
                    application.directory_generation,
                ) {
                    let origin =
                        if !application.playback_types.is_empty() || self.browser_focus_detached {
                            tag_filter::FilterOrigin::PassivePlayback
                        } else {
                            tag_filter::FilterOrigin::PassiveMetadata
                        };
                    tag_filter::invalidate_after_curation(self, origin, cx);
                }
            }
            if let Some(message) = self.curation_state.status_for(&key)
                && message.starts_with("Curation failed:")
            {
                self.scan_status = message;
            }
        }
        true
    }

    pub(super) fn active_curation_status(&self) -> Option<String> {
        let activation = self.catalog_activation.as_ref()?;
        self.curation_state.status_for(&CurationLifecycleKey {
            source_id: activation.source_id.clone(),
            lifecycle_generation: activation.lifecycle_generation,
        })
    }

    /// Return whether the active source has an accepted curation operation in flight.
    pub(super) fn curation_operation_pending(&self) -> bool {
        let Some(activation) = self.catalog_activation.as_ref() else {
            return false;
        };
        self.curation_state.has_pending(&CurationLifecycleKey {
            source_id: activation.source_id.clone(),
            lifecycle_generation: activation.lifecycle_generation,
        })
    }
}

fn curation_key_is_active(
    active_key: Option<&CurationLifecycleKey>,
    completed_key: &CurationLifecycleKey,
) -> bool {
    active_key.is_some_and(|active_key| active_key == completed_key)
}

fn shortcut_action(key: &str) -> Option<CurationAction> {
    match key {
        "[" => Some(CurationAction::AdjustRating { delta: -1 }),
        "]" => Some(CurationAction::AdjustRating { delta: 1 }),
        "1" => Some(CurationAction::ToggleCollection { slot: 0 }),
        "2" => Some(CurationAction::ToggleCollection { slot: 1 }),
        "3" => Some(CurationAction::ToggleCollection { slot: 2 }),
        "4" => Some(CurationAction::ToggleCollection { slot: 3 }),
        "5" => Some(CurationAction::ToggleCollection { slot: 4 }),
        "6" => Some(CurationAction::ToggleCollection { slot: 5 }),
        _ => None,
    }
}

pub(super) fn is_shortcut_key(key: &str) -> bool {
    shortcut_action(key).is_some()
}

fn patch_visible_entries(
    entries: &mut [AudioFile],
    patches: &BTreeMap<PathBuf, AudioFileCuration>,
    collection_filter: Option<usize>,
    rating_filter: RatingFilter,
    entry_indices: Option<&HashMap<PathBuf, usize>>,
) -> (bool, bool, bool, bool) {
    let mut collection_changed = false;
    let mut filter_membership_changed = false;
    let mut rating_filter_membership_changed = false;
    let mut last_played_changed = false;
    for (path, curation) in patches {
        let index = match entry_indices {
            Some(indices) => indices.get(path).copied(),
            None => entries
                .iter()
                .position(|entry| entry.relative_path == *path),
        };
        let Some(index) = index.filter(|index| *index < entries.len()) else {
            continue;
        };
        let entry = &mut entries[index];
        if entry.relative_path != *path {
            continue;
        }
        let previous = entry.curation;
        collection_changed |= previous
            .is_some_and(|previous| previous.collection_mask() != curation.collection_mask());
        filter_membership_changed |= collection_filter.is_some_and(|slot| {
            previous.is_some_and(|previous| previous.has_collection(slot))
                != curation.has_collection(slot)
        });
        rating_filter_membership_changed |= curation_matches_rating_filter(rating_filter, previous)
            != curation_matches_rating_filter(rating_filter, Some(*curation));
        last_played_changed |=
            previous.and_then(AudioFileCuration::last_played_at) != curation.last_played_at();
        entry.curation = Some(*curation);
    }
    (
        collection_changed,
        filter_membership_changed,
        rating_filter_membership_changed,
        last_played_changed,
    )
}

fn patch_visible_playback_types(
    entries: &mut [AudioFile],
    patches: &BTreeMap<PathBuf, Option<PlaybackType>>,
    filter: PlaybackTypeFilter,
    entry_indices: Option<&HashMap<PathBuf, usize>>,
) -> (bool, bool) {
    let mut playback_type_changed = false;
    let mut filter_membership_changed = false;
    for (path, playback_type) in patches {
        let index = match entry_indices {
            Some(indices) => indices.get(path).copied(),
            None => entries
                .iter()
                .position(|entry| entry.relative_path == *path),
        };
        let Some(index) = index.filter(|index| *index < entries.len()) else {
            continue;
        };
        let entry = &mut entries[index];
        if entry.relative_path != *path {
            continue;
        }
        let previous = entry.playback_type;
        playback_type_changed |= previous != *playback_type;
        filter_membership_changed |= filter.matches(previous) != filter.matches(*playback_type);
        entry.playback_type = *playback_type;
    }
    (playback_type_changed, filter_membership_changed)
}

fn curation_matches_rating_filter(
    filter: RatingFilter,
    curation: Option<AudioFileCuration>,
) -> bool {
    filter.is_disabled() || curation.is_some_and(|curation| filter.matches(curation.rating()))
}

fn is_playback_record_action(action: CurationAction) -> bool {
    matches!(action, CurationAction::RecordPlayback { .. })
}

fn explicit_playback_target_path(
    root: &Path,
    entry_indices: &HashMap<PathBuf, usize>,
    entries: &[AudioFile],
    absolute_path: &Path,
) -> Option<PathBuf> {
    let relative_path = absolute_path.strip_prefix(root).ok()?.to_path_buf();
    let entry_index = entry_indices.get(&relative_path).copied()?;
    let entry = entries.get(entry_index)?;
    (entry.path == absolute_path && entry.relative_path == relative_path).then_some(relative_path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PassiveProjectionFocus {
    Retain {
        visible_position: usize,
    },
    Override {
        entry_index: usize,
        visible_position: usize,
    },
    Detached,
}

fn passive_projection_focus(
    view_model: &BrowserViewModel,
    previous_id: Option<&AudioFileId>,
    previous_position: Option<usize>,
) -> PassiveProjectionFocus {
    if let Some(visible_position) = previous_id.and_then(|id| view_model.visible_position_for(id)) {
        return PassiveProjectionFocus::Retain { visible_position };
    }
    let Some(entry_index) = view_model.restore_focus(previous_id, previous_position) else {
        return PassiveProjectionFocus::Detached;
    };
    let Some(visible_position) = view_model
        .rows()
        .iter()
        .position(|row| row.entry_index == entry_index)
    else {
        return PassiveProjectionFocus::Detached;
    };
    PassiveProjectionFocus::Override {
        entry_index,
        visible_position,
    }
}

fn should_retain_passive_browser_focus(
    browser_focus_override: Option<&AudioFileId>,
    previous_focus_id: Option<&AudioFileId>,
    focused_row_visible: bool,
) -> bool {
    focused_row_visible
        && browser_focus_override.is_some()
        && browser_focus_override == previous_focus_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use yield_core::{BrowserViewModel, PlaybackLoopPolicy, RatingState};

    fn key() -> CurationLifecycleKey {
        CurationLifecycleKey {
            source_id: SourceId::from_string("curation-test-source"),
            lifecycle_generation: 1,
        }
    }

    fn curation(value: i8, locked: bool, collections: u8) -> AudioFileCuration {
        curation_with_last_played(value, locked, collections, Some(11))
    }

    fn curation_with_last_played(
        value: i8,
        locked: bool,
        collections: u8,
        last_played_at: Option<i64>,
    ) -> AudioFileCuration {
        AudioFileCuration::new(
            RatingState::new(value, locked).expect("valid test rating"),
            collections,
            last_played_at,
            Some(22),
        )
        .expect("valid test curation")
    }

    fn entry(name: &str, value: i8, locked: bool, collections: u8) -> AudioFile {
        AudioFile {
            path: PathBuf::from("/library").join(name),
            relative_path: PathBuf::from(name),
            byte_len: 1,
            duration_seconds: Some(1.0),
            sample_rate: Some(48_000),
            playback_type: None,
            curation: Some(curation(value, locked, collections)),
        }
    }

    fn install(entries: &[AudioFile]) -> CurationState {
        let mut state = CurationState::default();
        state.install_projection(
            key(),
            RootIdentity::from_bytes(b"root".to_vec()),
            2,
            10,
            entries,
        );
        state
    }

    #[test]
    fn display_label_reuse_classifies_real_curation_actions_conservatively() {
        let path = PathBuf::from("kick.wav");
        let state = install(&[entry("kick.wav", 0, false, 0)]);
        let playback = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 33 },
            std::slice::from_ref(&path),
        );
        assert!(!playback.tags_may_change());
        let rating = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        assert!(!rating.tags_may_change());

        let add = ready_tag_plan(&state, &[String::from("loop")], std::slice::from_ref(&path));
        assert!(add.tags_may_change());
        let remove = match state.plan_remove_tag(&key(), "loop", std::slice::from_ref(&path)) {
            CurationPlan::Ready(plan) => plan,
            CurationPlan::NoChange { .. } => panic!("expected a changing tag operation"),
            CurationPlan::Rejected(error) => panic!("unexpected rejection: {error}"),
        };
        assert!(remove.tags_may_change());

        // Any update outside the explicitly known metadata-only set remains conservative.
        let future_tag_update = SourceCurationUpdate::AddTag {
            path: RootRelativePath::try_from_path(path).expect("valid relative path"),
            label: String::from("future-tag"),
        };
        assert!(source_update_may_change_tags(&future_tag_update));
    }

    #[test]
    fn display_label_curation_gate_is_scoped_to_context_lifecycle() {
        let active_key = key();
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let plan = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 33 },
            std::slice::from_ref(&path),
        );
        assert!(state.accept_operation(&active_key, plan));
        let active_context = super::super::display_labels::DisplayLabelsContext {
            source_id: active_key.source_id.clone(),
            lifecycle_generation: active_key.lifecycle_generation,
            root: PathBuf::from("/library"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 2,
            revision: 10,
            entry_indices: std::sync::Arc::new(HashMap::from([(path.clone(), 0)])),
        };
        assert!(super::super::display_labels::curation_pending_for_context(
            &state,
            &active_context,
        ));
        let mut unrelated_context = active_context.clone();
        unrelated_context.source_id = SourceId::from_string("other-source");
        assert!(!super::super::display_labels::curation_pending_for_context(
            &state,
            &unrelated_context,
        ));
    }

    #[test]
    fn inactive_curation_receipt_is_not_active_for_tag_page_invalidation() {
        let active = key();
        let inactive = CurationLifecycleKey {
            source_id: SourceId::from_string("other-source"),
            lifecycle_generation: active.lifecycle_generation,
        };
        assert!(curation_key_is_active(Some(&active), &active));
        assert!(!curation_key_is_active(Some(&active), &inactive));
        assert!(!curation_key_is_active(None, &active));
    }

    fn ready_plan(
        state: &CurationState,
        action: CurationAction,
        paths: &[PathBuf],
    ) -> CurationOperationPlan {
        match state.plan_action(&key(), action, paths) {
            CurationPlan::Ready(plan) => plan,
            CurationPlan::NoChange { .. } => panic!("expected a changing operation"),
            CurationPlan::Rejected(error) => panic!("unexpected rejection: {error}"),
        }
    }

    fn ready_tag_plan(
        state: &CurationState,
        labels: &[String],
        paths: &[PathBuf],
    ) -> CurationOperationPlan {
        match state.plan_add_tags(&key(), labels, paths) {
            CurationPlan::Ready(plan) => plan,
            CurationPlan::NoChange { .. } => panic!("expected a changing tag operation"),
            CurationPlan::Rejected(error) => panic!("unexpected rejection: {error}"),
        }
    }

    fn receipt(revision: u64, path: &str, curation: AudioFileCuration) -> CurationReceipt {
        receipt_with_playback(revision, path, curation, None)
    }

    fn receipt_with_playback(
        revision: u64,
        path: &str,
        curation: AudioFileCuration,
        playback_type: Option<PlaybackType>,
    ) -> CurationReceipt {
        CurationReceipt {
            source_revision: revision,
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 2,
            files: BTreeMap::from([(PathBuf::from(path), curation)]),
            playback_types: BTreeMap::from([(PathBuf::from(path), playback_type)]),
            valid: true,
        }
    }

    fn invalid_receipt(revision: u64, path: &str, curation: AudioFileCuration) -> CurationReceipt {
        let mut receipt = receipt(revision, path, curation);
        receipt.valid = false;
        receipt
    }

    #[test]
    fn tag_plan_preserves_label_order_and_keeps_each_file_batch_together() {
        let paths = [PathBuf::from("first.wav"), PathBuf::from("second.wav")];
        let entries = [
            entry("first.wav", 0, false, 0),
            entry("second.wav", 0, false, 0),
        ];
        let state = install(&entries);
        let labels = vec![
            String::from("loop"),
            String::from("one-shot"),
            String::from("warm"),
        ];
        let plan = ready_tag_plan(&state, &labels, &paths);
        assert!(matches!(
            plan.fence,
            SourceCurationOperationFence::ExactRevision(10)
        ));
        assert!(plan.overlays.is_empty());
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].len(), paths.len() * labels.len());
        let updates = plan.batches[0]
            .iter()
            .map(|update| match update {
                SourceCurationUpdate::AddTag { path, label } => {
                    (path.as_path().to_path_buf(), label.clone())
                }
                _ => panic!("tag plan emitted a non-tag update"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            updates,
            vec![
                (PathBuf::from("first.wav"), String::from("loop")),
                (PathBuf::from("first.wav"), String::from("one-shot")),
                (PathBuf::from("first.wav"), String::from("warm")),
                (PathBuf::from("second.wav"), String::from("loop")),
                (PathBuf::from("second.wav"), String::from("one-shot")),
                (PathBuf::from("second.wav"), String::from("warm")),
            ]
        );
    }

    #[test]
    fn existing_tag_plan_emits_positive_id_without_label_identity() {
        let path = PathBuf::from("kick.wav");
        let state = install(&[entry("kick.wav", 0, false, 0)]);
        let plan = match state.plan_remove_tag_id(&key(), 41, std::slice::from_ref(&path)) {
            CurationPlan::Ready(plan) => plan,
            CurationPlan::NoChange { .. } => panic!("expected an existing-tag removal"),
            CurationPlan::Rejected(error) => panic!("unexpected rejection: {error}"),
        };
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].len(), 1);
        assert!(matches!(
            &plan.batches[0][0],
            SourceCurationUpdate::RemoveExistingTag { path: actual, tag_id }
                if actual.as_path() == path.as_path() && *tag_id == 41
        ));
        assert!(matches!(
            state.plan_remove_tag_id(&key(), 0, std::slice::from_ref(&path)),
            CurationPlan::Rejected(message) if message.contains("positive")
        ));
    }

    #[test]
    fn tag_plan_rejects_cartesian_overflow_before_admission() {
        let entries = (0..1025)
            .map(|index| entry(&format!("{index:04}.wav"), 0, false, 0))
            .collect::<Vec<_>>();
        let paths = entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect::<Vec<_>>();
        let labels = (0..MAX_TAGS_PER_COMMIT)
            .map(|index| format!("tag-{index}"))
            .collect::<Vec<_>>();
        let state = install(&entries);
        assert!(matches!(
            state.plan_add_tags(&key(), &labels, &paths),
            CurationPlan::Rejected(message) if message.contains("updates")
        ));
        assert!(!state.has_pending(&key()));
    }

    #[test]
    fn tag_plan_rejects_invalid_label_without_partial_updates() {
        let state = install(&[entry("kick.wav", 0, false, 0)]);
        let labels = vec![String::from("warm"), String::from("bad\0tag")];
        assert!(matches!(
            state.plan_add_tags(&key(), &labels, &[PathBuf::from("kick.wav")]),
            CurationPlan::Rejected(message) if message.contains("NUL")
        ));
        assert!(!state.has_pending(&key()));
    }

    #[test]
    fn tag_then_rating_collection_and_history_share_one_lineage() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let labels = vec![String::from("loop")];
        let tag = ready_tag_plan(&state, &labels, std::slice::from_ref(&path));
        let tag_id = tag.request_id;
        state.accept_operation(&key(), tag);

        let rating = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            rating.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == tag_id
        ));
        let rating_id = rating.request_id;
        state.accept_operation(&key(), rating);

        let collection = ready_plan(
            &state,
            CurationAction::ToggleCollection { slot: 0 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            collection.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == rating_id
        ));
        let collection_id = collection.request_id;
        state.accept_operation(&key(), collection);

        let history = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 33 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            history.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == collection_id
        ));
        let history_id = history.request_id;
        state.accept_operation(&key(), history);

        let tag_application = state
            .apply_outcome(
                key(),
                tag_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![receipt_with_playback(
                        11,
                        "kick.wav",
                        curation(0, false, 0),
                        Some(PlaybackType::Loop),
                    )],
                    failure: None,
                },
            )
            .expect("tag outcome is pending");
        assert_eq!(tag_application.prior_revision, 10);
        assert_eq!(tag_application.revision, 11);
        assert!(tag_application.tags_may_change);
        assert_eq!(
            tag_application.playback_types[&path],
            Some(PlaybackType::Loop)
        );
        assert_eq!(
            tag_application.visible[&path],
            curation_with_last_played(1, false, 1, Some(33))
        );

        let rating_application = state
            .apply_outcome(
                key(),
                rating_id,
                CurationOutcome {
                    initial_revision: Some(11),
                    final_revision: Some(12),
                    committed_batches: vec![receipt_with_playback(
                        12,
                        "kick.wav",
                        curation(1, false, 0),
                        Some(PlaybackType::Loop),
                    )],
                    failure: None,
                },
            )
            .expect("rating outcome is pending");
        assert_eq!(rating_application.prior_revision, 11);
        assert_eq!(rating_application.revision, 12);
        assert!(!rating_application.tags_may_change);
        assert_eq!(
            rating_application.visible[&path],
            curation_with_last_played(1, false, 1, Some(33))
        );

        let collection_application = state
            .apply_outcome(
                key(),
                collection_id,
                CurationOutcome {
                    initial_revision: Some(12),
                    final_revision: Some(13),
                    committed_batches: vec![receipt_with_playback(
                        13,
                        "kick.wav",
                        curation(1, false, 1),
                        Some(PlaybackType::Loop),
                    )],
                    failure: None,
                },
            )
            .expect("collection outcome is pending");
        assert_eq!(collection_application.prior_revision, 12);
        assert_eq!(collection_application.revision, 13);
        assert!(!collection_application.tags_may_change);
        assert_eq!(
            collection_application.visible[&path],
            curation_with_last_played(1, false, 1, Some(33))
        );

        let history_application = state
            .apply_outcome(
                key(),
                history_id,
                CurationOutcome {
                    initial_revision: Some(13),
                    final_revision: Some(14),
                    committed_batches: vec![receipt_with_playback(
                        14,
                        "kick.wav",
                        curation_with_last_played(1, false, 1, Some(33)),
                        Some(PlaybackType::Loop),
                    )],
                    failure: None,
                },
            )
            .expect("history outcome is pending");
        assert_eq!(history_application.prior_revision, 13);
        assert_eq!(history_application.revision, 14);
        assert!(!history_application.tags_may_change);
        assert_eq!(
            history_application.visible[&path],
            curation_with_last_played(1, false, 1, Some(33))
        );
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn rating_then_tag_and_history_preserve_predecessor_fences() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let rating = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let rating_id = rating.request_id;
        state.accept_operation(&key(), rating);
        let labels = vec![String::from("loop"), String::from("warm")];
        let tag = ready_tag_plan(&state, &labels, std::slice::from_ref(&path));
        let tag_id = tag.request_id;
        assert!(matches!(
            tag.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == rating_id
        ));
        state.accept_operation(&key(), tag);
        let history = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 44 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            history.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == tag_id
        ));
    }

    #[test]
    fn partial_tag_prefix_failure_applies_only_committed_receipt_types() {
        let entries = (0..129)
            .map(|index| entry(&format!("{index:03}.wav"), 0, false, 0))
            .collect::<Vec<_>>();
        let paths = entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect::<Vec<_>>();
        let labels = vec![String::from("loop"), String::from("warm")];
        let mut state = install(&entries);
        let plan = ready_tag_plan(&state, &labels, &paths);
        assert_eq!(plan.batches.len(), 3);
        let request_id = plan.request_id;
        state.accept_operation(&key(), plan);
        let application = state
            .apply_outcome(
                key(),
                request_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(12),
                    committed_batches: vec![
                        receipt_with_playback(
                            11,
                            "000.wav",
                            curation(0, false, 0),
                            Some(PlaybackType::Loop),
                        ),
                        receipt_with_playback(
                            12,
                            "001.wav",
                            curation(0, false, 0),
                            Some(PlaybackType::Loop),
                        ),
                    ],
                    failure: Some(String::from("stale source revision")),
                },
            )
            .expect("partial tag outcome is pending");
        assert!(application.failed);
        assert_eq!(application.revision, 12);
        assert_eq!(application.playback_types.len(), 2);
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn stale_tag_receipt_does_not_patch_playback_type() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let labels = vec![String::from("loop")];
        let plan = ready_tag_plan(&state, &labels, std::slice::from_ref(&path));
        let request_id = plan.request_id;
        state.accept_operation(&key(), plan);
        let application = state
            .apply_outcome(
                key(),
                request_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![invalid_receipt(11, "kick.wav", curation(0, false, 0))],
                    failure: None,
                },
            )
            .expect("stale tag outcome is pending");
        assert!(application.failed);
        assert!(application.playback_types.is_empty());
        assert_eq!(application.revision, 10);
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn non_projectable_committed_outcome_drains_without_advancing_projection() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let plan = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let request_id = plan.request_id;
        state.accept_operation(&key(), plan);

        let application = state
            .apply_outcome_with_projection(
                key(),
                request_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![receipt(11, "kick.wav", curation(1, false, 0))],
                    failure: None,
                },
                false,
            )
            .expect("non-projectable outcome is still consumed");
        assert!(!application.projectable);
        assert!(!application.metadata_changed);
        assert_eq!(application.revision, 10);
        assert!(!state.has_pending_outcomes());
        assert_eq!(
            state.lineages[&key()].confirmed[&path],
            curation(0, false, 0)
        );
        assert!(matches!(
            state.plan_action(
                &key(),
                CurationAction::AdjustRating { delta: 1 },
                std::slice::from_ref(&path),
            ),
            CurationPlan::Rejected(message) if message.contains("refresh the catalog")
        ));
        assert!(
            state
                .status_for(&key())
                .expect("non-projectable status")
                .contains("committed but cannot update")
        );
    }

    #[test]
    fn playback_target_uses_intent_path_even_when_browser_focus_differs() {
        let entries = [
            entry("focused.wav", 0, false, 0),
            entry("played.wav", 0, false, 0),
        ];
        let indices = HashMap::from([
            (PathBuf::from("focused.wav"), 0),
            (PathBuf::from("played.wav"), 1),
        ]);
        assert_eq!(
            explicit_playback_target_path(
                Path::new("/library"),
                &indices,
                &entries,
                Path::new("/library/played.wav"),
            ),
            Some(PathBuf::from("played.wav"))
        );
        assert_eq!(
            explicit_playback_target_path(
                Path::new("/library"),
                &indices,
                &entries,
                Path::new("/elsewhere/played.wav"),
            ),
            None
        );
    }

    #[test]
    fn rapid_keep_actions_chain_exact_predecessors_and_retain_lock() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let mut request_ids = Vec::new();
        for _ in 0..4 {
            let plan = ready_plan(
                &state,
                CurationAction::AdjustRating { delta: 1 },
                std::slice::from_ref(&path),
            );
            request_ids.push(plan.request_id);
            state.accept_operation(&key(), plan);
        }
        assert_eq!(request_ids, vec![1, 2, 3, 4]);
        assert!(matches!(state.lineages[&key()].pending[1].request_id, 2));
        assert_eq!(
            effective_curation(&state.lineages[&key()], &path),
            Some(curation(3, true, 0))
        );
        assert!(matches!(
            state.plan_action(
                &key(),
                CurationAction::AdjustRating { delta: 1 },
                std::slice::from_ref(&path),
            ),
            CurationPlan::NoChange { trash_required: 0 }
        ));
    }

    #[test]
    fn newer_optimistic_overlay_survives_earlier_success() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let first = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let first_id = first.request_id;
        state.accept_operation(&key(), first);
        let second = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let second_id = second.request_id;
        assert!(matches!(
            second.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == first_id
        ));
        state.accept_operation(&key(), second);

        let application = state
            .apply_outcome(
                key(),
                first_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![receipt(11, "kick.wav", curation(1, false, 0))],
                    failure: None,
                },
            )
            .expect("first outcome is pending");
        assert_eq!(application.revision, 11);
        assert_eq!(application.visible[&path], curation(2, false, 0));
        assert!(state.has_pending(&key()));

        let application = state
            .apply_outcome(
                key(),
                second_id,
                CurationOutcome {
                    initial_revision: Some(11),
                    final_revision: Some(12),
                    committed_batches: vec![receipt(12, "kick.wav", curation(2, false, 0))],
                    failure: None,
                },
            )
            .expect("second outcome is pending");
        assert_eq!(application.visible[&path], curation(2, false, 0));
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn history_then_rating_overlay_preserves_both_fields_through_receipts() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let history = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 33 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            history.batches[0].as_slice(),
            [SourceCurationUpdate::SetLastPlayedAt {
                played_at: Some(33),
                ..
            }]
        ));
        assert!(!history.tags_may_change());
        let history_id = history.request_id;
        state.accept_operation(&key(), history);

        let rating = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let rating_id = rating.request_id;
        assert!(matches!(
            rating.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == history_id
        ));
        state.accept_operation(&key(), rating);

        let application = state
            .apply_outcome(
                key(),
                history_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![receipt(
                        11,
                        "kick.wav",
                        curation_with_last_played(0, false, 0, Some(33)),
                    )],
                    failure: None,
                },
            )
            .expect("history outcome is pending");
        assert_eq!(application.prior_revision, 10);
        assert_eq!(application.revision, 11);
        assert!(!application.tags_may_change);
        assert_eq!(
            application.visible[&path],
            curation_with_last_played(1, false, 0, Some(33))
        );

        let application = state
            .apply_outcome(
                key(),
                rating_id,
                CurationOutcome {
                    initial_revision: Some(11),
                    final_revision: Some(12),
                    committed_batches: vec![receipt(
                        12,
                        "kick.wav",
                        curation_with_last_played(1, false, 0, Some(33)),
                    )],
                    failure: None,
                },
            )
            .expect("rating outcome is pending");
        assert_eq!(application.prior_revision, 11);
        assert_eq!(application.revision, 12);
        assert!(!application.tags_may_change);
        assert_eq!(
            application.visible[&path],
            curation_with_last_played(1, false, 0, Some(33))
        );
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn rating_then_history_overlay_preserves_both_fields_through_receipts() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let rating = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let rating_id = rating.request_id;
        state.accept_operation(&key(), rating);

        let history = ready_plan(
            &state,
            CurationAction::RecordPlayback { played_at: 33 },
            std::slice::from_ref(&path),
        );
        let history_id = history.request_id;
        assert!(matches!(
            history.fence,
            SourceCurationOperationFence::AfterOperation { request_id } if request_id == rating_id
        ));
        state.accept_operation(&key(), history);

        let application = state
            .apply_outcome(
                key(),
                rating_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(11),
                    committed_batches: vec![receipt(
                        11,
                        "kick.wav",
                        curation_with_last_played(1, false, 0, Some(11)),
                    )],
                    failure: None,
                },
            )
            .expect("rating outcome is pending");
        assert_eq!(
            application.visible[&path],
            curation_with_last_played(1, false, 0, Some(33))
        );

        let application = state
            .apply_outcome(
                key(),
                history_id,
                CurationOutcome {
                    initial_revision: Some(11),
                    final_revision: Some(12),
                    committed_batches: vec![receipt(
                        12,
                        "kick.wav",
                        curation_with_last_played(1, false, 0, Some(33)),
                    )],
                    failure: None,
                },
            )
            .expect("history outcome is pending");
        assert_eq!(
            application.visible[&path],
            curation_with_last_played(1, false, 0, Some(33))
        );
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn failed_lineage_rolls_back_and_invalidates_dependents() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let first = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let first_id = first.request_id;
        state.accept_operation(&key(), first);
        let second = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let second_id = second.request_id;
        state.accept_operation(&key(), second);

        let application = state
            .apply_outcome(
                key(),
                first_id,
                CurationOutcome {
                    initial_revision: Some(10),
                    final_revision: Some(10),
                    committed_batches: Vec::new(),
                    failure: Some(String::from("stale revision")),
                },
            )
            .expect("failed operation is pending");
        assert!(application.failed);
        assert_eq!(application.visible[&path], curation(0, false, 0));
        assert!(state.has_pending(&key()));
        assert!(
            state
                .status_for(&key())
                .expect("failure status")
                .contains("refresh the catalog")
        );

        let application = state
            .apply_outcome(
                key(),
                second_id,
                CurationOutcome {
                    initial_revision: None,
                    final_revision: None,
                    committed_batches: Vec::new(),
                    failure: Some(String::from("predecessor unavailable")),
                },
            )
            .expect("dependent terminal outcome is drained");
        assert!(application.failed);
        assert!(!state.has_pending_outcomes());
    }

    #[test]
    fn rating_and_lock_updates_stay_in_one_batch_and_large_selection_batches() {
        let mut entries = vec![entry("locked.wav", 3, false, 0)];
        entries.extend((0..129).map(|index| entry(&format!("{index:03}.wav"), 0, false, 0)));
        let mut state = install(&entries);
        let lock_plan = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            &[PathBuf::from("locked.wav")],
        );
        assert_eq!(lock_plan.batches[0].len(), 2);
        assert!(matches!(
            lock_plan.batches[0][0],
            SourceCurationUpdate::SetRating { .. }
        ));
        assert!(matches!(
            lock_plan.batches[0][1],
            SourceCurationUpdate::SetLocked { .. }
        ));
        state.accept_operation(&key(), lock_plan);

        let paths = (0..129)
            .map(|index| PathBuf::from(format!("{index:03}.wav")))
            .collect::<Vec<_>>();
        let plan = ready_plan(&state, CurationAction::ToggleCollection { slot: 0 }, &paths);
        assert_eq!(plan.batches.len(), 2);
        assert_eq!(plan.batches[0].len(), 128);
        assert_eq!(plan.batches[1].len(), 1);
    }

    #[test]
    fn mixed_collection_toggle_and_trash_requirement_are_explicit() {
        let entries = [
            entry("already.wav", -3, false, 0),
            entry("keep.wav", 0, false, 0),
        ];
        let state = install(&entries);
        let plan = state.plan_action(
            &key(),
            CurationAction::AdjustRating { delta: -1 },
            &[PathBuf::from("already.wav"), PathBuf::from("keep.wav")],
        );
        let CurationPlan::Ready(plan) = plan else {
            panic!("the non-trash row should still be admitted");
        };
        assert_eq!(plan.trash_required, 1);
        assert_eq!(plan.batches.iter().map(Vec::len).sum::<usize>(), 1);

        let plan = state.plan_action(
            &key(),
            CurationAction::ToggleCollection { slot: 2 },
            &[PathBuf::from("already.wav"), PathBuf::from("keep.wav")],
        );
        let CurationPlan::Ready(plan) = plan else {
            panic!("both rows should toggle independently");
        };
        assert_eq!(plan.batches.iter().map(Vec::len).sum::<usize>(), 2);
    }

    #[test]
    fn collection_toggle_remains_independent_for_locked_rating() {
        let path = PathBuf::from("locked.wav");
        let state = install(&[entry("locked.wav", 3, true, 0)]);
        let plan = state.plan_action(
            &key(),
            CurationAction::ToggleCollection { slot: 2 },
            std::slice::from_ref(&path),
        );
        let CurationPlan::Ready(plan) = plan else {
            panic!("locked rows should still accept collection edits");
        };
        assert!(matches!(
            plan.batches[0].as_slice(),
            [SourceCurationUpdate::AddCollection { collection, .. }]
                if collection.index() == 2
        ));
    }

    #[test]
    fn remove_collection_action_targets_only_the_active_slot() {
        let path = PathBuf::from("member.wav");
        let state = install(&[entry("member.wav", 1, false, 0b0000_0101)]);
        let plan = ready_plan(
            &state,
            CurationAction::RemoveCollection { slot: 2 },
            std::slice::from_ref(&path),
        );
        assert!(matches!(
            plan.batches[0].as_slice(),
            [SourceCurationUpdate::RemoveCollection { collection, .. }]
                if collection.index() == 2
        ));
        assert!(matches!(
            state.plan_action(
                &key(),
                CurationAction::RemoveCollection { slot: 1 },
                std::slice::from_ref(&path),
            ),
            CurationPlan::NoChange { trash_required: 0 }
        ));
    }

    #[test]
    fn invalid_collection_slot_is_a_no_op_without_submission() {
        let path = PathBuf::from("kick.wav");
        let state = install(&[entry("kick.wav", 1, false, 0b0000_0011)]);
        assert!(matches!(
            state.plan_action(
                &key(),
                CurationAction::ToggleCollection {
                    slot: COLLECTION_COUNT,
                },
                std::slice::from_ref(&path),
            ),
            CurationPlan::NoChange { trash_required: 0 }
        ));
    }

    #[test]
    fn no_op_locked_rating_does_not_build_an_operation() {
        let state = install(&[entry("locked.wav", 2, true, 0)]);
        assert!(matches!(
            state.plan_action(
                &key(),
                CurationAction::AdjustRating { delta: -1 },
                &[PathBuf::from("locked.wav")],
            ),
            CurationPlan::NoChange { trash_required: 0 }
        ));
    }

    #[test]
    fn superseded_projection_drains_old_outcome_without_patching_new_state() {
        let path = PathBuf::from("kick.wav");
        let mut state = install(&[entry("kick.wav", 0, false, 0)]);
        let plan = ready_plan(
            &state,
            CurationAction::AdjustRating { delta: 1 },
            std::slice::from_ref(&path),
        );
        let request_id = plan.request_id;
        state.accept_operation(&key(), plan);
        state.install_projection(
            key(),
            RootIdentity::from_bytes(b"root".to_vec()),
            2,
            20,
            &[entry("kick.wav", 2, false, 0)],
        );
        assert!(state.has_pending_outcomes());
        assert!(
            state
                .apply_outcome(
                    key(),
                    request_id,
                    CurationOutcome {
                        initial_revision: Some(10),
                        final_revision: Some(11),
                        committed_batches: vec![receipt(11, "kick.wav", curation(1, false, 0))],
                        failure: None,
                    },
                )
                .is_none()
        );
        assert!(!state.has_pending_outcomes());
        assert_eq!(
            state.lineages[&key()].confirmed[&path],
            curation(2, false, 0)
        );
    }

    #[test]
    fn shortcut_mapping_contains_only_plain_curation_keys() {
        assert!(is_shortcut_key("["));
        assert!(is_shortcut_key("]"));
        assert!(is_shortcut_key("1"));
        assert!(is_shortcut_key("6"));
        assert!(!is_shortcut_key("0"));
        assert!(!is_shortcut_key("u"));
    }

    #[test]
    fn rating_curation_patch_rebuilds_projection_and_counts_when_rows_leave_and_rejoin_filter() {
        let mut entries = vec![
            entry("alpha.wav", 0, false, 0b0000_0001),
            entry("bravo.wav", 0, false, 0b0000_0010),
        ];
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_rating_filter(&entries, 0));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(model.collection_counts(&entries), [1, 1, 0, 0, 0, 0]);

        let alpha = PathBuf::from("alpha.wav");
        let changed_alpha = curation(1, false, 0b0000_0001);
        let patches = BTreeMap::from([(alpha.clone(), changed_alpha)]);
        let (
            collection_changed,
            collection_membership_changed,
            rating_membership_changed,
            last_played_changed,
        ) = patch_visible_entries(
            &mut entries,
            &patches,
            model.collection_filter(),
            model.rating_filter(),
            None,
        );
        assert!(!collection_changed);
        assert!(!collection_membership_changed);
        assert!(rating_membership_changed);
        assert!(!last_played_changed);
        assert_eq!(entries[0].curation, Some(changed_alpha));

        model.set_entries(&entries);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(model.collection_counts(&entries), [0, 1, 0, 0, 0, 0]);

        let restored_alpha = curation(0, false, 0b0000_0001);
        let patches = BTreeMap::from([(alpha, restored_alpha)]);
        let (_, _, rating_membership_changed, last_played_changed) = patch_visible_entries(
            &mut entries,
            &patches,
            model.collection_filter(),
            model.rating_filter(),
            None,
        );
        assert!(rating_membership_changed);
        assert!(!last_played_changed);
        model.set_entries(&entries);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(model.collection_counts(&entries), [1, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn curation_patch_preserves_projected_playback_type() {
        let path = PathBuf::from("loop.wav");
        let mut entries = vec![entry("loop.wav", 0, false, 0)];
        entries[0].playback_type = Some(PlaybackType::Loop);
        let patches = BTreeMap::from([(path, curation(1, false, 0b0000_0001))]);
        patch_visible_entries(&mut entries, &patches, None, RatingFilter::disabled(), None);

        assert_eq!(entries[0].playback_type, Some(PlaybackType::Loop));
        assert_eq!(entries[0].curation, Some(curation(1, false, 0b0000_0001)));
    }

    #[test]
    fn playback_type_receipt_patch_updates_filter_counts_and_sort() {
        let mut entries = vec![
            entry("one.wav", 0, false, 0b0000_0001),
            entry("loop.wav", 0, false, 0b0000_0010),
        ];
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));
        let patches = BTreeMap::from([
            (PathBuf::from("one.wav"), Some(PlaybackType::OneShot)),
            (PathBuf::from("loop.wav"), Some(PlaybackType::Loop)),
        ]);
        let (type_changed, membership_changed) = patch_visible_playback_types(
            &mut entries,
            &patches,
            model.playback_type_filter(),
            None,
        );
        assert!(type_changed);
        assert!(membership_changed);
        model.set_entries(&entries);
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 1);
        assert_eq!(model.collection_counts(&entries), [0, 1, 0, 0, 0, 0]);

        model.set_playback_type_filter(&entries, PlaybackTypeFilter::disabled());
        model.toggle_sort(&entries, yield_core::SortColumn::PlaybackType);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
    }

    #[test]
    fn last_played_curation_patch_reorders_history_sorted_projection() {
        let mut entries = vec![
            entry("alpha.wav", 0, false, 0),
            entry("bravo.wav", 0, false, 0),
        ];
        entries[0].curation = AudioFileCuration::from_parts(0, false, 0, Some(1), None);
        entries[1].curation = AudioFileCuration::from_parts(0, false, 0, Some(2), None);
        let mut model = BrowserViewModel::new(&entries);
        model.toggle_sort(&entries, yield_core::SortColumn::LastPlayed);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );

        let alpha = PathBuf::from("alpha.wav");
        let updated_alpha =
            AudioFileCuration::from_parts(0, false, 0, Some(3), None).expect("valid curation");
        let patches = BTreeMap::from([(alpha, updated_alpha)]);
        let (_, _, _, last_played_changed) =
            patch_visible_entries(&mut entries, &patches, None, RatingFilter::disabled(), None);
        assert!(last_played_changed);
        model.set_entries(&entries);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn passive_type_projection_detaches_empty_focus_and_keeps_loaded_policy() {
        let mut entries = vec![entry("one.wav", 0, false, 0)];
        entries[0].playback_type = Some(PlaybackType::OneShot);
        let focused_id = entries[0].stable_id();
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));

        entries[0].playback_type = Some(PlaybackType::Loop);
        model.set_entries(&entries);
        assert_eq!(model.rows().len(), 0);
        assert_eq!(
            passive_projection_focus(&model, Some(&focused_id), Some(0)),
            PassiveProjectionFocus::Detached
        );

        let mut policy = PlaybackLoopPolicy::default();
        policy.prepare(focused_id.clone(), Some(PlaybackType::OneShot));
        policy.set_manual(true);
        policy.prepare(focused_id, Some(PlaybackType::Loop));
        assert!(policy.enabled());
        assert!(policy.has_manual_override());
    }

    #[test]
    fn passive_type_projection_keeps_detached_focus_for_followup_metadata_receipts() {
        let mut entries = vec![entry("one.wav", 0, false, 0)];
        entries[0].playback_type = Some(PlaybackType::OneShot);
        let focused_id = entries[0].stable_id();
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));

        entries[0].playback_type = Some(PlaybackType::Loop);
        model.set_entries(&entries);
        assert_eq!(
            passive_projection_focus(&model, Some(&focused_id), Some(0)),
            PassiveProjectionFocus::Detached
        );

        // A later rating/history receipt does not change the type-filter projection. Once the
        // loaded row is detached, it must remain detached rather than entering the old restore
        // path that clears the audition when it sees no visible row.
        assert_eq!(
            passive_projection_focus(&model, None, None),
            PassiveProjectionFocus::Detached
        );
    }

    #[test]
    fn passive_type_projection_uses_visible_override_without_reselecting_loaded_row() {
        let mut entries = vec![
            entry("loaded.wav", 0, false, 0),
            entry("other.wav", 0, false, 0),
        ];
        entries[0].playback_type = Some(PlaybackType::OneShot);
        entries[1].playback_type = Some(PlaybackType::OneShot);
        let loaded_id = entries[0].stable_id();
        let other_id = entries[1].stable_id();
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));

        entries[0].playback_type = Some(PlaybackType::Loop);
        model.set_entries(&entries);
        assert_eq!(model.rows().len(), 1);
        assert_eq!(
            passive_projection_focus(&model, Some(&loaded_id), Some(0)),
            PassiveProjectionFocus::Override {
                entry_index: 1,
                visible_position: 0,
            }
        );
        assert_ne!(loaded_id, other_id);
        assert_eq!(model.rows()[0].id, other_id);

        entries[0].playback_type = Some(PlaybackType::OneShot);
        model.set_entries(&entries);
        assert_eq!(
            passive_projection_focus(&model, Some(&loaded_id), Some(0)),
            PassiveProjectionFocus::Retain {
                visible_position: 0,
            }
        );
    }

    #[test]
    fn metadata_reprojection_retains_passive_focus_only_while_visible() {
        let passive = entry("passive.wav", 0, false, 0).stable_id();
        let loaded = entry("loaded.wav", 0, false, 0).stable_id();
        assert!(should_retain_passive_browser_focus(
            Some(&passive),
            Some(&passive),
            true
        ));
        assert!(!should_retain_passive_browser_focus(
            Some(&passive),
            Some(&passive),
            false
        ));
        assert!(!should_retain_passive_browser_focus(
            Some(&passive),
            Some(&loaded),
            true
        ));
        assert!(!should_retain_passive_browser_focus(
            None,
            Some(&passive),
            true
        ));
    }
}
