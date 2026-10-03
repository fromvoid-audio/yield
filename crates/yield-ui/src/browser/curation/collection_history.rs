//! Bounded collection and sample lock history acknowledged by the existing catalog owner.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use gpui::{Pixels, Point};
use yield_core::BrowserSelection;

use super::*;

const MAX_RECORDS: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// One explicit history direction; inverse writes never toggle membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Undo,
    Redo,
}

/// Stable presentation observations, never persistence authority.
#[derive(Clone, Debug, Default)]
pub(super) struct Presentation {
    pub(super) selection: BrowserSelection,
    pub(super) focus: Option<AudioFileId>,
    pub(super) viewport_anchor: Option<AudioFileId>,
    pub(super) viewport_offset: Point<Pixels>,
    pub(super) viewport_index: usize,
}

impl Presentation {
    pub(super) fn estimated_bytes(&self) -> usize {
        self.selection
            .selected()
            .iter()
            .chain(self.selection.anchor())
            .chain(self.focus.iter())
            .chain(self.viewport_anchor.iter())
            .fold(
                512usize.saturating_add(
                    self.selection
                        .selected()
                        .capacity()
                        .saturating_mul(std::mem::size_of::<AudioFileId>() + 32),
                ),
                |bytes, id| bytes.saturating_add(128 + path_byte_len(id.path())),
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Collection(usize),
    UnlockKeepRating,
}

impl Field {
    fn value(self, row: AudioFileCuration) -> Option<bool> {
        match self {
            Self::Collection(slot) => Some(row.has_collection(slot)),
            Self::UnlockKeepRating => (row.rating().value() == 3).then_some(row.rating().locked()),
        }
    }
    fn update(self, path: RootRelativePath, value: bool) -> Option<SourceCurationUpdate> {
        Some(match self {
            Self::Collection(slot) => {
                let collection = SampleCollection::new(slot as u8)?;
                if value {
                    SourceCurationUpdate::AddCollection { path, collection }
                } else {
                    SourceCurationUpdate::RemoveCollection { path, collection }
                }
            }
            Self::UnlockKeepRating => SourceCurationUpdate::SetLocked {
                path,
                locked: value,
            },
        })
    }
    fn overlay(self, current: AudioFileCuration, value: bool) -> Option<AudioFileCuration> {
        match self {
            Self::Collection(slot) => {
                let CurationTransition::Changed(row) =
                    plan_curation_action(current, CurationAction::ToggleCollection { slot })
                else {
                    return None;
                };
                Some(row)
            }
            Self::UnlockKeepRating => AudioFileCuration::from_parts(
                3,
                value,
                current.collection_mask(),
                current.last_played_at(),
                current.last_curated_at(),
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Record {
    key: CurationLifecycleKey,
    root: RootIdentity,
    generation: u64,
    field: Field,
    origin_request: u64,
    bits: BTreeMap<PathBuf, (bool, bool)>,
    presentation: Presentation,
    bytes: usize,
    owner: Option<Arc<yield_session::CatalogCollectionHistoryGroup>>,
    applied: BTreeSet<PathBuf>,
    partial: bool,
    phase: Option<Direction>,
}

struct Pending {
    request: u64,
    initial_revision: u64,
    direction: Option<Direction>,
    record: Record,
    prepared: Option<Arc<yield_session::CatalogCollectionHistoryGroup>>,
    batches: Vec<Vec<PathBuf>>,
}

/// One bounded history lane. Replacing its audited source context retires the lane.
#[derive(Default)]
pub(super) struct History {
    undo: VecDeque<Record>,
    redo: VecDeque<Record>,
    revision: Option<u64>,
    pending: Option<Pending>,
    known_failure: bool,
}

impl History {
    pub(super) fn is_pending(&self, key: &CurationLifecycleKey, request: u64) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.request == request && p.record.key == *key)
    }
    pub(super) fn pending_key(&self) -> Option<CurationLifecycleKey> {
        self.pending.as_ref().map(|p| p.record.key.clone())
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn retire_except(
        &mut self,
        active: Option<&CurationLifecycleKey>,
    ) -> Option<CurationLifecycleKey> {
        if self.key().is_some_and(|key| active != Some(key)) {
            let unresolved = self
                .pending
                .as_ref()
                .map(|pending| pending.record.key.clone());
            self.clear();
            unresolved
        } else {
            None
        }
    }

    fn key(&self) -> Option<&CurationLifecycleKey> {
        self.pending
            .as_ref()
            .map(|pending| &pending.record.key)
            .or_else(|| self.undo.back().map(|record| &record.key))
            .or_else(|| self.redo.back().map(|record| &record.key))
    }

    pub(super) fn invalidate_key(&mut self, key: &CurationLifecycleKey) {
        if self.key() == Some(key) {
            self.clear();
        }
    }

    fn bytes(&self) -> usize {
        self.undo
            .iter()
            .chain(&self.redo)
            .map(|record| record.bytes)
            .sum::<usize>()
            .saturating_add(
                self.pending
                    .as_ref()
                    .filter(|p| p.direction.is_none())
                    .map_or(0, |p| p.record.bytes),
            )
    }

    pub(super) fn reserve_preparation(&self, bytes: usize, direction: Option<Direction>) -> bool {
        // Preparation/admission failure cannot move settled stack positions. Refuse coverage
        // before allocating when retaining the old lane plus this capture cannot fit.
        self.pending.is_none()
            && bytes <= MAX_BYTES
            && self.bytes().saturating_add(bytes) <= MAX_BYTES
            && (direction.is_some() || self.undo.len() + self.redo.len() < MAX_RECORDS)
    }
    fn top(&self, direction: Direction) -> Option<&Record> {
        if self.undo.back().is_some_and(|r| r.partial) {
            return self.undo.back();
        }
        match direction {
            Direction::Undo => self.undo.back(),
            Direction::Redo => self.redo.back(),
        }
    }

    pub(super) fn ready(
        &self,
        key: &CurationLifecycleKey,
        lineage: &CurationLineage,
        direction: Direction,
    ) -> bool {
        self.pending.is_none()
            && lineage.pending.is_empty()
            && lineage.staged_latest.is_none()
            && (lineage.failed.is_none() || self.permits_failure(key, lineage))
            && !lineage.history_unresolved
            && lineage.invalidated.is_none()
            && self.revision == Some(lineage.revision)
            && self.top(direction).is_some_and(|record| {
                (!record.partial || record.owner.is_some())
                    && record.key == *key
                    && record.root == lineage.root_identity
                    && record.generation == lineage.directory_generation
            })
    }

    pub(super) fn plan(
        &self,
        key: &CurationLifecycleKey,
        lineage: &CurationLineage,
        request_id: u64,
        direction: Direction,
    ) -> Option<CurationOperationPlan> {
        if !self.ready(key, lineage, direction) {
            return None;
        }
        let record = self.top(direction)?;
        let mut updates = Vec::new();
        let mut overlays = BTreeMap::new();
        for (path, (before, after)) in &record.bits {
            let current = *lineage.confirmed.get(path)?;
            let expected = if record.applied.contains(path) {
                *after
            } else {
                *before
            };
            let target = match direction {
                Direction::Undo => *before,
                Direction::Redo => *after,
            };
            if record.field.value(current)? != expected {
                return None;
            }
            let relative = RootRelativePath::try_from_path(path.clone()).ok()?;
            updates.push(record.field.update(relative, target)?);
            if expected != target {
                overlays.insert(path.clone(), record.field.overlay(current, target)?);
            }
        }
        Some(CurationOperationPlan {
            request_id,
            fence: SourceCurationOperationFence::ExactRevision(lineage.revision),
            root_identity: lineage.root_identity.clone(),
            batches: updates
                .chunks(MAX_CURATION_UPDATES)
                .map(|c| c.to_vec())
                .collect(),
            overlays,
            trash_required: 0,
            history_direction: Some(direction),
            prepared_group: None,
            history_unavailable: false,
            captured_presentation: None,
        })
    }

    pub(super) fn accept(
        &mut self,
        key: &CurationLifecycleKey,
        lineage: &CurationLineage,
        plan: &CurationOperationPlan,
        presentation: Option<Presentation>,
    ) {
        if let Some(direction) = plan.history_direction {
            if self.ready(key, lineage, direction)
                && let Some(record) = self.top(direction).cloned()
            {
                self.pending = Some(Pending {
                    request: plan.request_id,
                    initial_revision: lineage.revision,
                    direction: Some(direction),
                    record,
                    prepared: plan.prepared_group.clone(),
                    batches: plan
                        .batches
                        .iter()
                        .map(|b| {
                            b.iter()
                                .filter_map(|u| match u {
                                    SourceCurationUpdate::AddCollection { path, .. }
                                    | SourceCurationUpdate::RemoveCollection { path, .. }
                                    | SourceCurationUpdate::SetLocked { path, .. } => {
                                        Some(path.as_path().to_path_buf())
                                    }
                                    _ => None,
                                })
                                .collect()
                        })
                        .collect(),
                });
            }
            return;
        }
        if self.pending.is_some()
            || !lineage.pending.is_empty()
            || lineage.staged_latest.is_some()
            || plan.fence != SourceCurationOperationFence::ExactRevision(lineage.revision)
            || plan.history_unavailable
            || plan.batches.is_empty()
            || (plan.prepared_group.is_none() && plan.batches.len() != 1)
            || plan
                .batches
                .iter()
                .any(|b| b.is_empty() || b.len() > MAX_CURATION_UPDATES)
        {
            return;
        }
        let Some(presentation) = presentation else {
            return;
        };
        let estimate = plan
            .overlays
            .keys()
            .fold(presentation.estimated_bytes(), |bytes, path| {
                bytes.saturating_add(256 + path_byte_len(path))
            })
            .saturating_add(key.source_id.as_str().len())
            .saturating_add(lineage.root_identity.as_bytes().len())
            .saturating_mul(3);
        let estimate = estimate.saturating_add(
            plan.prepared_group
                .as_ref()
                .map_or(0, |g| g.source_observation().charged_bytes()),
        );
        let sample = plan
            .batches
            .iter()
            .flatten()
            .all(|u| matches!(u, SourceCurationUpdate::SetLocked { locked: false, .. }));
        if estimate > MAX_BYTES || (sample && estimate > 1024 * 1024) {
            return;
        }
        let mut bits = BTreeMap::new();
        let mut field = None;
        for update in plan.batches.iter().flatten() {
            let (path, this_field, after) = match update {
                SourceCurationUpdate::AddCollection { path, collection } => (
                    path,
                    Field::Collection(usize::from(collection.index())),
                    true,
                ),
                SourceCurationUpdate::RemoveCollection { path, collection } => (
                    path,
                    Field::Collection(usize::from(collection.index())),
                    false,
                ),
                SourceCurationUpdate::SetLocked {
                    path,
                    locked: false,
                } if plan.batches.len() == 1 => (path, Field::UnlockKeepRating, false),
                _ => return,
            };
            if field.is_some_and(|prior| prior != this_field) {
                return;
            }
            field = Some(this_field);
            let Some(current) = lineage.confirmed.get(path.as_path()) else {
                return;
            };
            let Some(before) = this_field.value(*current) else {
                return;
            };
            if before == after
                || bits
                    .insert(path.as_path().to_path_buf(), (before, after))
                    .is_some()
            {
                return;
            }
        }
        // Charge three copies for settled + in-flight direction + presentation restoration.
        // Fixed allowances include container/node overhead; variable identities are charged too.
        let bytes = bits
            .keys()
            .fold(presentation.estimated_bytes(), |bytes, path| {
                bytes.saturating_add(256 + path_byte_len(path))
            })
            .saturating_add(key.source_id.as_str().len())
            .saturating_add(lineage.root_identity.as_bytes().len())
            .saturating_mul(3);
        let bytes = bytes.saturating_add(
            plan.prepared_group
                .as_ref()
                .map_or(0, |g| g.source_observation().charged_bytes()),
        );
        if bytes > MAX_BYTES || (field == Some(Field::UnlockKeepRating) && bytes > 1024 * 1024) {
            return;
        }
        while self.bytes().saturating_add(bytes) > MAX_BYTES
            || self.undo.len() + self.redo.len() >= MAX_RECORDS
        {
            if self.undo.front().is_some_and(|r| r.partial)
                || self.redo.front().is_some_and(|r| r.partial)
            {
                return;
            }
            if self.undo.pop_front().is_none() {
                // An uncommitted original never evicts redo to make room.
                return;
            }
        }
        self.pending = Some(Pending {
            request: plan.request_id,
            initial_revision: lineage.revision,
            direction: None,
            prepared: plan.prepared_group.clone(),
            batches: plan
                .batches
                .iter()
                .map(|b| {
                    b.iter()
                        .filter_map(|u| match u {
                            SourceCurationUpdate::AddCollection { path, .. }
                            | SourceCurationUpdate::RemoveCollection { path, .. }
                            | SourceCurationUpdate::SetLocked { path, .. } => {
                                Some(path.as_path().to_path_buf())
                            }
                            _ => None,
                        })
                        .collect()
                })
                .collect(),
            record: Record {
                key: key.clone(),
                root: lineage.root_identity.clone(),
                generation: lineage.directory_generation,
                field: field.expect("non-empty history batch"),
                origin_request: plan.request_id,
                applied: bits.keys().cloned().collect(),
                bits,
                owner: plan.prepared_group.clone(),
                partial: false,
                phase: None,
                presentation,
                bytes,
            },
        });
    }

    pub(super) fn owner_matches_direction(
        &self,
        plan: &CurationOperationPlan,
        group: &yield_session::CatalogCollectionHistoryGroup,
    ) -> bool {
        let Some(direction) = plan.history_direction else {
            return true;
        };
        let Some(record) = self.top(direction) else {
            return false;
        };
        let Some(owner) = record.owner.as_ref() else {
            return false;
        };
        let source = group.source_observation();
        if !source.same_owner_as(owner.source_observation())
            || source.changes().len() > record.bits.len()
        {
            return false;
        }
        let changed = source
            .changes()
            .iter()
            .map(|c| (c.path().as_path(), c))
            .collect::<BTreeMap<_, _>>();
        if changed.keys().any(|p| !record.bits.contains_key(*p)) {
            return false;
        }
        record.bits.iter().all(|(p, (before, after))| {
            let expected = if record.applied.contains(p) {
                *after
            } else {
                *before
            };
            let target = match direction {
                Direction::Undo => *before,
                Direction::Redo => *after,
            };
            match changed.get(p.as_path()) {
                Some(c) => c.before() == expected && c.after() == target && expected != target,
                None => expected == target,
            }
        })
    }

    pub(super) fn permits_failure(
        &self,
        key: &CurationLifecycleKey,
        lineage: &CurationLineage,
    ) -> bool {
        self.known_failure
            && self.revision == Some(lineage.revision)
            && self.key() == Some(key)
            && !lineage.history_unresolved
            && self
                .undo
                .iter()
                .chain(&self.redo)
                .any(|r| r.owner.is_some())
    }

    pub(super) fn control_label(&self, direction: Direction) -> &'static str {
        match self.undo.back().filter(|r| r.partial).map(|r| r.phase) {
            Some(Some(Direction::Undo)) => match direction {
                Direction::Undo => "Resume undo",
                Direction::Redo => "Reverse undo",
            },
            Some(Some(Direction::Redo)) => match direction {
                Direction::Undo => "Reverse redo",
                Direction::Redo => "Resume redo",
            },
            Some(None) => match direction {
                Direction::Undo => "Undo confirmed",
                Direction::Redo => "Keep confirmed",
            },
            None => match direction {
                Direction::Undo
                    if self
                        .top(direction)
                        .is_some_and(|r| r.field == Field::UnlockKeepRating) =>
                {
                    "Undo unlock"
                }
                Direction::Redo
                    if self
                        .top(direction)
                        .is_some_and(|r| r.field == Field::UnlockKeepRating) =>
                {
                    "Redo unlock"
                }
                Direction::Undo => "Undo collection",
                Direction::Redo => "Redo collection",
            },
        }
    }

    pub(super) fn control_snapshot(
        &self,
        direction: Direction,
    ) -> Option<(CurationLifecycleKey, u64, u64, Option<Direction>)> {
        let record = self.top(direction)?;
        Some((
            record.key.clone(),
            self.revision?,
            record.origin_request,
            record.phase,
        ))
    }

    fn pop_direction_record(&mut self, direction: Direction) -> Option<Record> {
        if self.undo.back().is_some_and(|r| r.partial) || direction == Direction::Undo {
            self.undo.pop_back()
        } else {
            self.redo.pop_back()
        }
    }

    /// A genuine successful all-path refence can establish an already-reached endpoint locally.
    /// This moves no metadata and manufactures no receipt.
    pub(super) fn complete_empty_direction(
        &mut self,
        plan: &CurationOperationPlan,
        group: &yield_session::CatalogCollectionHistoryGroup,
    ) -> Option<Presentation> {
        let direction = plan.history_direction?;
        if !self.owner_matches_direction(plan, group)
            || !group.source_observation().changes().is_empty()
        {
            return None;
        }
        let mut record = self.pop_direction_record(direction)?;
        record.partial = false;
        record.phase = None;
        let presentation = record.presentation.clone();
        match direction {
            Direction::Undo => {
                record.applied.clear();
                self.redo.push_back(record);
            }
            Direction::Redo => {
                record.applied = record.bits.keys().cloned().collect();
                self.undo.push_back(record);
            }
        }
        self.known_failure = false;
        Some(presentation)
    }

    pub(super) fn validate_bound_outcome(
        &self,
        key: &CurationLifecycleKey,
        request: u64,
        outcome: &CurationOutcome,
    ) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| p.request == request && p.record.key == *key)
        else {
            return outcome.prepared_group.is_none();
        };
        Self::valid_receipts(pending, outcome)
    }
    fn valid_receipts(pending: &Pending, outcome: &CurationOutcome) -> bool {
        if outcome.initial_revision != Some(pending.initial_revision)
            || outcome.committed_batches.len() > pending.batches.len()
            || outcome.final_revision
                != pending
                    .initial_revision
                    .checked_add(outcome.committed_batches.len() as u64)
        {
            return false;
        }
        match (&pending.prepared, &outcome.prepared_group) {
            (Some(expected), Some(actual)) => {
                if !Arc::ptr_eq(expected, actual) {
                    return false;
                }
                let source = expected.source_observation();
                let rev = source.revision();
                if rev.source_id() != &pending.record.key.source_id
                    || rev.lifecycle_generation() != pending.record.key.lifecycle_generation
                    || rev.root_identity() != &pending.record.root
                    || rev.revision() != pending.initial_revision
                    || source.directory_generation() != pending.record.generation
                {
                    return false;
                }
                if outcome.failure.is_some()
                    && (outcome.failure_index != Some(outcome.committed_batches.len())
                        || outcome.failure_error.is_none())
                {
                    return false;
                }
            }
            (None, None) => {}
            _ => return false,
        }
        if outcome.failure.is_none() {
            if outcome.committed_batches.len() != pending.batches.len()
                || outcome.failure_index.is_some()
                || outcome.failure_error.is_some()
            {
                return false;
            }
        } else if outcome.committed_batches.len() >= pending.batches.len()
            || outcome
                .failure_index
                .is_some_and(|i| i != outcome.committed_batches.len())
        {
            return false;
        }
        outcome
            .committed_batches
            .iter()
            .enumerate()
            .all(|(index, r)| {
                let paths = &pending.batches[index];
                r.valid
                    && r.source_revision == pending.initial_revision + index as u64 + 1
                    && r.root_identity == pending.record.root
                    && r.directory_generation == pending.record.generation
                    && r.files.len() == paths.len()
                    && paths.iter().all(|p| {
                        let Some((before, after)) = pending.record.bits.get(p) else {
                            return false;
                        };
                        let target = if pending.direction == Some(Direction::Undo) {
                            *before
                        } else {
                            *after
                        };
                        r.files
                            .get(p)
                            .is_some_and(|f| pending.record.field.value(*f) == Some(target))
                    })
            })
    }

    pub(super) fn complete(
        &mut self,
        key: &CurationLifecycleKey,
        request: u64,
        outcome: &CurationOutcome,
        lineage_valid: bool,
    ) -> (Option<Presentation>, Option<String>) {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request == request && pending.record.key == *key)
        {
            if !outcome.committed_batches.is_empty() {
                self.invalidate_key(key);
            }
            return (None, None);
        }
        let mut pending = self.pending.take().expect("matching pending history");
        let valid = lineage_valid && Self::valid_receipts(&pending, outcome);
        if !valid {
            self.clear();
            return (
                None,
                Some(
                    "Curation undo unavailable: result could not be confirmed; refresh the catalog"
                        .into(),
                ),
            );
        }
        if outcome.failure.is_some() {
            self.known_failure = true;
            self.revision = outcome.final_revision;
        }
        if outcome.committed_batches.is_empty()
            && outcome.failure.is_some()
            && pending.direction.is_none()
        {
            return (
                None,
                Some(format!(
                    "Collection save failed: 0/{} batches confirmed · 0 changed paths committed · {} remain; {} · directions unavailable",
                    pending.batches.len(),
                    pending.batches.iter().map(Vec::len).sum::<usize>(),
                    outcome.failure.as_deref().unwrap_or("batch failed")
                )),
            );
        }
        if outcome.failure.is_some() && pending.record.field == Field::UnlockKeepRating {
            return (
                None,
                Some("Sample unlock failed; refresh the catalog before retrying".into()),
            );
        }
        if outcome.failure.is_some() {
            let committed = outcome.committed_batches.len();
            let total = pending.batches.len();
            let paths = pending
                .batches
                .iter()
                .take(committed)
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>();
            match pending.direction {
                None => {
                    pending.record.bits.retain(|p, _| paths.contains(p));
                    pending.record.applied = paths.clone();
                    self.redo.clear();
                }
                Some(Direction::Undo) => {
                    for p in &paths {
                        pending.record.applied.remove(p);
                    }
                    self.pop_direction_record(Direction::Undo);
                }
                Some(Direction::Redo) => {
                    pending.record.applied.extend(paths.iter().cloned());
                    self.pop_direction_record(Direction::Redo);
                }
            }
            pending.record.partial = true;
            pending.record.phase = pending.direction;
            self.revision = outcome.final_revision;
            let message = format!(
                "Collection partially saved: {committed}/{total} batches · {} changed paths committed · {} remain; {} · resume or reverse confirmed changes",
                paths.len(),
                pending
                    .batches
                    .iter()
                    .skip(committed)
                    .map(Vec::len)
                    .sum::<usize>(),
                outcome.failure.as_deref().unwrap_or("batch failed")
            );
            self.undo.push_back(pending.record);
            return (None, Some(message));
        }
        self.revision = outcome.final_revision;
        self.known_failure = false;
        let sample = pending.record.field == Field::UnlockKeepRating;
        let (restore, message) = match pending.direction {
            None => {
                self.redo.clear();
                if self.undo.back().is_some_and(|record| {
                    record.key != *key
                        || record.root != pending.record.root
                        || record.generation != pending.record.generation
                }) {
                    self.undo.clear();
                }
                self.undo.push_back(pending.record);
                (
                    None,
                    if sample {
                        "Sample unlocked · Undo available"
                    } else {
                        "Collection saved · Undo available"
                    },
                )
            }
            Some(Direction::Undo) => {
                self.pop_direction_record(Direction::Undo);
                pending.record.partial = false;
                pending.record.phase = None;
                let restore = pending.record.presentation.clone();
                pending.record.applied.clear();
                self.redo.push_back(pending.record);
                (
                    Some(restore),
                    if sample {
                        "Sample unlock undo saved · Redo available"
                    } else {
                        "Collection undo saved · Redo available"
                    },
                )
            }
            Some(Direction::Redo) => {
                self.pop_direction_record(Direction::Redo);
                pending.record.partial = false;
                pending.record.phase = None;
                let restore = pending.record.presentation.clone();
                pending.record.applied = pending.record.bits.keys().cloned().collect();
                self.undo.push_back(pending.record);
                (
                    Some(restore),
                    if sample {
                        "Sample unlock redo saved · Undo available"
                    } else {
                        "Collection redo saved · Undo available"
                    },
                )
            }
        };
        (restore, Some(message.into()))
    }
}

#[cfg(test)]
mod tests;
