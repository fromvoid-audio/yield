//! Bounded collection membership history acknowledged by the existing catalog owner.

use std::collections::VecDeque;

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
            .fold(512usize, |bytes, id| {
                bytes.saturating_add(128 + path_byte_len(id.path()))
            })
    }
}

#[derive(Clone, Debug)]
struct Record {
    key: CurationLifecycleKey,
    root: RootIdentity,
    generation: u64,
    slot: usize,
    bits: BTreeMap<PathBuf, (bool, bool)>,
    presentation: Presentation,
    bytes: usize,
}

struct Pending {
    request: u64,
    initial_revision: u64,
    direction: Option<Direction>,
    record: Record,
}

/// One bounded history lane. Replacing its audited source context retires the lane.
#[derive(Default)]
pub(super) struct History {
    undo: VecDeque<Record>,
    redo: VecDeque<Record>,
    revision: Option<u64>,
    pending: Option<Pending>,
}

impl History {
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

    fn top(&self, direction: Direction) -> Option<&Record> {
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
            && lineage.failed.is_none()
            && lineage.invalidated.is_none()
            && self.revision == Some(lineage.revision)
            && self.top(direction).is_some_and(|record| {
                record.key == *key
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
        let collection = SampleCollection::new(record.slot as u8)?;
        let mut updates = Vec::new();
        let mut overlays = BTreeMap::new();
        for (path, (before, after)) in &record.bits {
            let current = *lineage.confirmed.get(path)?;
            let (expected, target) = match direction {
                Direction::Undo => (*after, *before),
                Direction::Redo => (*before, *after),
            };
            if current.has_collection(record.slot) != expected || expected == target {
                return None;
            }
            let relative = RootRelativePath::try_from_path(path.clone()).ok()?;
            updates.push(if target {
                SourceCurationUpdate::AddCollection {
                    path: relative,
                    collection,
                }
            } else {
                SourceCurationUpdate::RemoveCollection {
                    path: relative,
                    collection,
                }
            });
            let CurationTransition::Changed(changed) = plan_curation_action(
                current,
                CurationAction::ToggleCollection { slot: record.slot },
            ) else {
                return None;
            };
            overlays.insert(path.clone(), changed);
        }
        Some(CurationOperationPlan {
            request_id,
            fence: SourceCurationOperationFence::ExactRevision(lineage.revision),
            root_identity: lineage.root_identity.clone(),
            batches: vec![updates],
            overlays,
            trash_required: 0,
            history_direction: Some(direction),
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
                });
            }
            return;
        }
        if self.pending.is_some()
            || !lineage.pending.is_empty()
            || lineage.staged_latest.is_some()
            || plan.fence != SourceCurationOperationFence::ExactRevision(lineage.revision)
            || plan.batches.len() != 1
            || plan.batches[0].is_empty()
            || plan.batches[0].len() > MAX_CURATION_UPDATES
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
        if estimate > MAX_BYTES {
            return;
        }
        let mut bits = BTreeMap::new();
        let mut slot = None;
        for update in &plan.batches[0] {
            let (path, collection, after) = match update {
                SourceCurationUpdate::AddCollection { path, collection } => {
                    (path, collection, true)
                }
                SourceCurationUpdate::RemoveCollection { path, collection } => {
                    (path, collection, false)
                }
                _ => return,
            };
            let this_slot = usize::from(collection.index());
            if slot.is_some_and(|prior| prior != this_slot) {
                return;
            }
            slot = Some(this_slot);
            let Some(current) = lineage.confirmed.get(path.as_path()) else {
                return;
            };
            let before = current.has_collection(this_slot);
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
        if bytes > MAX_BYTES {
            return;
        }
        while self.bytes().saturating_add(bytes) > MAX_BYTES
            || self.undo.len() + self.redo.len() >= MAX_RECORDS
        {
            if self.undo.pop_front().is_none() && self.redo.pop_front().is_none() {
                break;
            }
        }
        self.pending = Some(Pending {
            request: plan.request_id,
            initial_revision: lineage.revision,
            direction: None,
            record: Record {
                key: key.clone(),
                root: lineage.root_identity.clone(),
                generation: lineage.directory_generation,
                slot: slot.expect("non-empty collection batch"),
                bits,
                presentation,
                bytes,
            },
        });
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
        let pending = self.pending.take().expect("matching pending history");
        if outcome.committed_batches.is_empty() && outcome.failure.is_some() {
            return (None, None);
        }
        let valid = lineage_valid
            && outcome.failure.is_none()
            && outcome.initial_revision == Some(pending.initial_revision)
            && outcome.committed_batches.len() == 1
            && outcome.final_revision == pending.initial_revision.checked_add(1)
            && outcome.committed_batches.first().is_some_and(|receipt| {
                receipt.valid
                    && Some(receipt.source_revision) == pending.initial_revision.checked_add(1)
                    && receipt.root_identity == pending.record.root
                    && receipt.directory_generation == pending.record.generation
                    && receipt.files.len() == pending.record.bits.len()
                    && pending.record.bits.iter().all(|(path, (before, after))| {
                        let target = if pending.direction == Some(Direction::Undo) {
                            *before
                        } else {
                            *after
                        };
                        receipt
                            .files
                            .get(path)
                            .is_some_and(|file| file.has_collection(pending.record.slot) == target)
                    })
            });
        if !valid {
            self.clear();
            return (
                None,
                Some(
                    "Collection undo unavailable: result could not be confirmed; refresh the catalog"
                        .into(),
                ),
            );
        }
        self.revision = outcome.final_revision;
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
                (None, "Collection saved · Undo available")
            }
            Some(Direction::Undo) => {
                self.undo.pop_back();
                let restore = pending.record.presentation.clone();
                self.redo.push_back(pending.record);
                (Some(restore), "Collection undo saved · Redo available")
            }
            Some(Direction::Redo) => {
                self.redo.pop_back();
                let restore = pending.record.presentation.clone();
                self.undo.push_back(pending.record);
                (Some(restore), "Collection redo saved · Undo available")
            }
        };
        (restore, Some(message.into()))
    }
}

#[cfg(test)]
mod tests;
