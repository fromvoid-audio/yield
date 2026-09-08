#![deny(missing_docs)]
#![deny(warnings)]

//! Passive curation metadata projection and source-scoped collection filters.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use gpui::{ClickEvent, Context, ScrollStrategy, div, prelude::*, px};
use wavecrate_core::{
    AudioFile, AudioFileCuration, AudioFileId, COLLECTION_COUNT, CurationAction,
    CurationTransition, RATING_FILTER_LEVELS, RatingFilter, plan_curation_action,
};
use wavecrate_design::Theme;
use wavecrate_library::sample_sources::{
    BrowserFileMetadata, Rating, SampleCollection, SourceCurationUpdate, SourceId,
    reconciliation::{RootIdentity, RootRelativePath},
};
use wavecrate_session::{
    MAX_CURATION_OPERATION_BATCHES, MAX_CURATION_OPERATION_PATH_BYTES,
    MAX_CURATION_OPERATION_RECEIPT_BYTES, MAX_CURATION_OPERATION_UPDATES, MAX_CURATION_UPDATES,
    SourceCurationCommitReceipt, SourceCurationOperationFence, SourceCurationOperationOutcome,
};

use crate::theme::{color, transparent};

use super::{Browser, catalog};

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
    overlays: BTreeMap<PathBuf, AudioFileCuration>,
    active_overlay: bool,
    trash_required: usize,
}

#[derive(Clone, Debug)]
struct CurationLineage {
    root_identity: RootIdentity,
    directory_generation: u64,
    revision: u64,
    confirmed: BTreeMap<PathBuf, AudioFileCuration>,
    pending: Vec<PendingCurationOperation>,
    failed: Option<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct CurationReceipt {
    source_revision: u64,
    root_identity: RootIdentity,
    directory_generation: u64,
    files: BTreeMap<PathBuf, AudioFileCuration>,
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
    pub(super) revision: u64,
    pub(super) visible: BTreeMap<PathBuf, AudioFileCuration>,
    pub(super) metadata_changed: bool,
    pub(super) failed: bool,
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
        if lineage.failed.is_some() || plan.root_identity != lineage.root_identity {
            return false;
        }
        if plan.request_id != next_request_id {
            return false;
        }
        self.next_request_id = plan.request_id;
        lineage.message = None;
        lineage.pending.push(PendingCurationOperation {
            request_id: plan.request_id,
            batch_count: plan.batches.len(),
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
        let (batch_count, operation_trash_required, operation_active, operation_paths) = {
            let operation = &lineage.pending[operation_index];
            (
                operation.batch_count,
                operation.trash_required,
                operation.active_overlay,
                operation.overlays.keys().cloned().collect::<HashSet<_>>(),
            )
        };
        let expected_initial = outcome.initial_revision;
        let mut next_revision = lineage.revision;
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
        for receipt in receipt_updates {
            for (path, curation) in &receipt.files {
                lineage.confirmed.insert(path.clone(), *curation);
            }
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
        let metadata_changed = !visible.is_empty();
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
            revision: lineage.revision,
            visible,
            metadata_changed,
            failed: failed.is_some(),
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
        let mut valid = true;
        for metadata in receipt.files() {
            let Some(curation) = curation_from_metadata(metadata) else {
                valid = false;
                continue;
            };
            if files
                .insert(metadata.relative_path.clone(), curation)
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
        CurationAction::UnlockKeepRating => (current.rating().locked() != after.rating().locked())
            .then_some(SourceCurationUpdate::SetLocked {
                path,
                locked: after.rating().locked(),
            })
            .into_iter()
            .collect(),
    }
}

fn path_byte_len(path: &PathBuf) -> usize {
    path.to_string_lossy().len()
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
    let chip_width = ((theme.spacing.sidebar_width
        - theme.spacing.panel_padding * 2.0
        - theme.spacing.control_gap * (RATING_FILTER_LEVELS.len().saturating_sub(1) as f32))
        / RATING_FILTER_LEVELS.len() as f32)
        .max(18.0);
    let chips = RATING_FILTER_LEVELS
        .into_iter()
        .enumerate()
        .map(|(index, level)| {
            let selected = active_filter.contains_level(level);
            let marker_color = color(rating_filter_color(level, &theme));
            let label = rating_filter_label(level);
            div()
                .id(("rating-filter", index))
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
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if active_source {
                        view.toggle_rating_filter(level, cx);
                    }
                }))
                .child(
                    div()
                        .flex_none()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded_full()
                        .bg(marker_color),
                )
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
                .child("RATING"),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .w_full()
                .px(px(theme.spacing.panel_padding))
                .gap(px(theme.spacing.control_gap))
                .children(chips),
        )
}

fn rating_filter_label(level: i8) -> &'static str {
    match level {
        -3 => "T3",
        -2 => "T2",
        -1 => "T1",
        0 => "U",
        1 => "K1",
        2 => "K2",
        3 => "K3",
        4 => "K4",
        _ => "?",
    }
}

fn rating_filter_color(level: i8, theme: &Theme) -> wavecrate_design::Color {
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
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.browser_focus_override = None;
        self.context_menu = None;
        if !self.view_model.toggle_rating_filter(&self.entries, level) {
            return;
        }
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }

    pub(super) fn toggle_collection_filter(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.active_source.is_none() {
            return;
        }
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        self.browser_focus_override = None;
        self.context_menu = None;
        if !self
            .view_model
            .toggle_collection_filter(&self.entries, slot)
        {
            return;
        }
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_projection_focus(previous_id, previous_position, cx);
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

    pub(super) fn submit_curation_action(
        &mut self,
        action: CurationAction,
        explicit_paths: Option<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        let Some(activation) = self.catalog_activation.as_ref() else {
            self.scan_status =
                String::from("Curation requires a successful audited catalog projection");
            cx.notify();
            return;
        };
        if !activation.audit_finished || activation.audit_error.is_some() {
            self.scan_status =
                String::from("Curation requires a successful audited catalog projection");
            cx.notify();
            return;
        }
        let Some(context) = self.catalog_metadata.context() else {
            self.scan_status = String::from(
                "Curation metadata is still loading; try again after the catalog is ready",
            );
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
                String::from("Curation authority is stale; refresh the catalog before editing"),
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
                let optimistic = plan.overlays.clone();
                let command = wavecrate_session::CatalogCommand::ApplyCurationOperation {
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
                        } else {
                            self.catalog_metadata.pause_for_curation();
                            self.apply_visible_curation(&key, &optimistic, cx);
                            self.start_source_registry_poll(cx);
                        }
                    }
                    Err(error) => {
                        self.curation_state
                            .set_message(&key, format!("Curation not saved: {error}"));
                    }
                }
            }
        }
        cx.notify();
    }

    fn curation_target_paths(&self) -> Vec<PathBuf> {
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

    fn apply_visible_curation(
        &mut self,
        key: &CurationLifecycleKey,
        patches: &BTreeMap<PathBuf, AudioFileCuration>,
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
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        let collection_filter = self.view_model.collection_filter();
        let rating_filter = self.view_model.rating_filter();
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
        if collection_changed || rating_filter_membership_changed {
            self.refresh_collection_filter_counts();
        }
        if sort_column == wavecrate_core::SortColumn::Rating
            || sort_column == wavecrate_core::SortColumn::Collection
            || (sort_column == wavecrate_core::SortColumn::LastPlayed && last_played_changed)
            || filter_membership_changed
            || rating_filter_membership_changed
        {
            self.view_model.set_entries(&self.entries);
            self.reconcile_selection();
            self.restore_metadata_projection_focus(previous_id, previous_position, cx);
        }
    }

    fn restore_metadata_projection_focus(
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
        self.browser_focus_override = None;
        self.restore_projection_focus(previous_id, previous_position, cx);
    }

    /// Consume a compound catalog outcome before active-source event filtering.
    pub(super) fn apply_curation_event(
        &mut self,
        event: &wavecrate_session::CatalogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let wavecrate_session::CatalogEvent::CurationOperationCompleted {
            source_id,
            lifecycle_generation,
            request_id,
            outcome,
        } = event
        else {
            return false;
        };
        let key = CurationLifecycleKey {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        };
        let outcome = CurationOutcome::from_catalog(outcome);
        let Some(application) =
            self.curation_state
                .apply_outcome(key.clone(), *request_id, outcome)
        else {
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
        let active = self.catalog_activation.as_ref().is_some_and(|activation| {
            activation.source_id == key.source_id
                && activation.lifecycle_generation == key.lifecycle_generation
        });
        if active {
            if application.metadata_changed {
                self.apply_visible_curation(&key, &application.visible, cx);
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

fn curation_matches_rating_filter(
    filter: RatingFilter,
    curation: Option<AudioFileCuration>,
) -> bool {
    filter.is_disabled() || curation.is_some_and(|curation| filter.matches(curation.rating()))
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
    use wavecrate_core::{BrowserViewModel, RatingState};

    fn key() -> CurationLifecycleKey {
        CurationLifecycleKey {
            source_id: SourceId::from_string("curation-test-source"),
            lifecycle_generation: 1,
        }
    }

    fn curation(value: i8, locked: bool, collections: u8) -> AudioFileCuration {
        AudioFileCuration::new(
            RatingState::new(value, locked).expect("valid test rating"),
            collections,
            Some(11),
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

    fn receipt(revision: u64, path: &str, curation: AudioFileCuration) -> CurationReceipt {
        CurationReceipt {
            source_revision: revision,
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 2,
            files: BTreeMap::from([(PathBuf::from(path), curation)]),
            valid: true,
        }
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
    fn last_played_curation_patch_reorders_history_sorted_projection() {
        let mut entries = vec![
            entry("alpha.wav", 0, false, 0),
            entry("bravo.wav", 0, false, 0),
        ];
        entries[0].curation = AudioFileCuration::from_parts(0, false, 0, Some(1), None);
        entries[1].curation = AudioFileCuration::from_parts(0, false, 0, Some(2), None);
        let mut model = BrowserViewModel::new(&entries);
        model.toggle_sort(&entries, wavecrate_core::SortColumn::LastPlayed);
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
