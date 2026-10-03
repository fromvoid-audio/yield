//! Genuine bounded multi-chunk collection observations and owner-bound batch progression.
use super::*;
use crate::app_dirs::WritableProfileGuard;
use crate::sample_sources::{SourceCurationCommitReceipt, SourceCurationUpdate};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

/// Maximum logical updates; ordinary catalog admission limits are unchanged.
pub const SOURCE_COLLECTION_HISTORY_GROUP_MAX_PATHS: usize = 65_536;
/// Conservative combined preparation/history-copy bound; larger edits lack history coverage.
pub const SOURCE_COLLECTION_HISTORY_GROUP_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Private genuine owner observation for a deterministic compound collection edit.
///
/// Cloning an Arc to this data grants no SQL, journal, capacity or ownership permission.
pub struct SourceCollectionHistoryGroup {
    revision: SourceRevision,
    directory_generation: u64,
    collection: SampleCollection,
    changes: Vec<SourceCollectionHistoryChange>,
    deadline: Instant,
    lease_expires_at: SystemTime,
    owner: Weak<()>,
    bytes: usize,
}
impl std::fmt::Debug for SourceCollectionHistoryGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCollectionHistoryGroup")
            .field("changed_paths", &self.changes.len())
            .field("charged_bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
impl SourceCollectionHistoryGroup {
    /// Complete original owner/revision observation, never a caller-bumped write revision.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Exact audited directory generation.
    pub fn directory_generation(&self) -> u64 {
        self.directory_generation
    }
    /// One explicitly selected collection slot.
    pub fn collection(&self) -> SampleCollection {
        self.collection
    }
    /// Sorted changed membership observations; original no-ops are omitted.
    pub fn changes(&self) -> &[SourceCollectionHistoryChange] {
        &self.changes
    }
    /// Deterministic immutable partitions, each at the ordinary 128-update source limit.
    pub fn batches(&self) -> impl ExactSizeIterator<Item = &[SourceCollectionHistoryChange]> {
        self.changes.chunks(MAX_PATHS)
    }
    /// Compare genuine original issuer and complete stable identity without SQL or renewal.
    /// Numeric revisions may differ; this observation comparison grants no mutation permission.
    pub fn same_owner_as(&self, other: &Self) -> bool {
        self.owner.ptr_eq(&other.owner)
            && self.owner.strong_count() != 0
            && self.revision.source_id() == other.revision.source_id()
            && self.revision.root_identity() == other.revision.root_identity()
            && self.revision.root_binding_identity() == other.revision.root_binding_identity()
            && self.revision.database_identity() == other.revision.database_identity()
            && self.revision.lifecycle_generation() == other.revision.lifecycle_generation()
            && self.revision.lease_epoch() == other.revision.lease_epoch()
            && self.directory_generation == other.directory_generation
    }
    /// Conservative charge for retained input/output and later history copies.
    pub fn charged_bytes(&self) -> usize {
        self.bytes
    }
    /// In-memory clock/issuer liveness only; mutation separately checks actual owner state.
    pub fn within_observation_window(&self) -> bool {
        Instant::now() < self.deadline
            && SystemTime::now() < self.lease_expires_at
            && self.owner.strong_count() != 0
    }
}

/// Non-cloneable progression issued only after actual same-owner initial validation.
///
/// This keeps original owner provenance across genuine acknowledged batch revisions.
/// It is neither journal admission nor a capacity/commit permit, and cannot cross threads.
pub struct SourceCollectionHistoryExecution {
    group: Arc<SourceCollectionHistoryGroup>,
    next_batch: usize,
    revision: u64,
    _thread: PhantomData<Rc<()>>,
}
impl std::fmt::Debug for SourceCollectionHistoryExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCollectionHistoryExecution")
            .field("next_batch", &self.next_batch)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl SourceCollectionHistoryExecution {
    /// Exact next private batch ordinal, advanced only by a genuine successful receipt.
    pub fn next_batch(&self) -> usize {
        self.next_batch
    }
    /// Exact revision from initial observation or last genuine acknowledged batch.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Original observation retained for outcome custody, without fresh authority.
    pub fn group(&self) -> &Arc<SourceCollectionHistoryGroup> {
        &self.group
    }
}

impl SourceWriter {
    /// Capture one bounded immutable compound group on this actual retained owner.
    ///
    /// Reuses real 128-row selection reads, at one full revision/directory and original
    /// cooperative deadline. No mutation, owner renewal, journal identity or grant occurs.
    pub fn capture_collection_history_group(
        &self,
        expected: &SourceRevision,
        collection: SampleCollection,
        desired: bool,
        paths: &[RootRelativePath],
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SourceCollectionHistoryGroup, SourceWriterError> {
        Self::collection_group_charge(expected, paths.iter().map(|p| p.as_path()), paths.len())?;
        let targets: Vec<_> = paths.iter().map(|p| (p.clone(), desired)).collect();
        self.capture_collection_history_mixed_group(
            expected, collection, &targets, cancel, deadline,
        )
    }

    fn collection_group_charge<'a>(
        expected: &SourceRevision,
        paths: impl Iterator<Item = &'a Path>,
        count: usize,
    ) -> Result<usize, SourceWriterError> {
        if count == 0 || count > SOURCE_COLLECTION_HISTORY_GROUP_MAX_PATHS {
            return Err(SourceWriterError::InvalidRequest("collection group paths"));
        }
        if expected.source_id().as_str().len() > 256
            || expected.root_identity().as_bytes().len() > 4096
            || expected.root_binding_identity().len() > 512
            || expected.database_identity().len() > 512
        {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let mut bytes = 64usize * 1024;
        for path in paths {
            let len = path.as_os_str().len();
            bytes = bytes
                .checked_add(
                    len.checked_mul(8)
                        .and_then(|n| n.checked_add(1024))
                        .ok_or(SourceWriterError::SnapshotBudgetExceeded)?,
                )
                .ok_or(SourceWriterError::SnapshotBudgetExceeded)?;
            if len > MAX_PATH_BYTES || bytes > SOURCE_COLLECTION_HISTORY_GROUP_MAX_BYTES {
                return Err(SourceWriterError::SnapshotBudgetExceeded);
            }
        }
        Ok(bytes)
    }

    /// Capture exact per-path explicit target bits, including mixed Add/Remove groups.
    /// Each real 128-row observation shares the original full owner and cooperative deadline.
    pub fn capture_collection_history_mixed_group(
        &self,
        expected: &SourceRevision,
        collection: SampleCollection,
        targets: &[(RootRelativePath, bool)],
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SourceCollectionHistoryGroup, SourceWriterError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        let checkpoint = || {
            if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                Err(SourceWriterError::SnapshotBudgetExceeded)
            } else {
                Ok(())
            }
        };
        checkpoint()?;
        let bytes = Self::collection_group_charge(
            expected,
            targets.iter().map(|(p, _)| p.as_path()),
            targets.len(),
        )?;
        let mut sorted = Vec::with_capacity(targets.len());
        for (path, desired) in targets {
            checkpoint()?;
            let key = normalize_relative_path(path.as_path())
                .map_err(|_| SourceWriterError::InvalidRequest("collection group path"))?;
            sorted.push((key, path, *desired));
        }
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if sorted.windows(2).any(|p| p[0].0 == p[1].0) {
            return Err(SourceWriterError::InvalidRequest(
                "duplicate collection group path",
            ));
        }
        let mut changes = Vec::new();
        let mut directory = None;
        let mut lease_expires_at = SystemTime::now() + Duration::from_secs(2);
        let mut effective_deadline = deadline;
        for chunk in sorted.chunks(MAX_PATHS) {
            for desired in [false, true] {
                checkpoint()?;
                let paths: Vec<_> = chunk
                    .iter()
                    .filter(|(_, _, d)| *d == desired)
                    .map(|(_, p, _)| (*p).clone())
                    .collect();
                if paths.is_empty() {
                    continue;
                }
                let observed = self.capture_collection_history_selection_with_deadline(
                    expected, collection, desired, &paths, deadline,
                )?;
                if &observed.revision != expected
                    || directory.is_some_and(|n| n != observed.directory_generation)
                    || !observed.within_observation_window()
                {
                    return Err(SourceWriterError::StaleFence);
                }
                directory = Some(observed.directory_generation);
                effective_deadline = effective_deadline.min(observed.observation_deadline);
                lease_expires_at = lease_expires_at.min(observed.lease_expires_at);
                changes.extend(observed.changes);
            }
        }
        let mut keyed = Vec::with_capacity(changes.len());
        for change in changes {
            let key = normalize_relative_path(change.path.as_path())
                .map_err(|_| SourceWriterError::StaleFence)?;
            keyed.push((key, change));
        }
        keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let changes = keyed.into_iter().map(|(_, c)| c).collect();
        checkpoint()?;
        if self.current_revision()? != *expected {
            return Err(SourceWriterError::StaleFence);
        }
        let group = SourceCollectionHistoryGroup {
            revision: expected.clone(),
            directory_generation: directory.ok_or(SourceWriterError::StaleFence)?,
            collection,
            changes,
            deadline: effective_deadline,
            lease_expires_at,
            owner: Arc::downgrade(&self.reference_liveness),
            bytes,
        };
        if !group.within_observation_window() {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(group)
    }

    /// Validate original observation and issue private same-owner batch progression.
    /// No write, renewed owner, operation/intent identity or capacity permission is issued.
    pub fn begin_collection_history_execution(
        &self,
        group: Arc<SourceCollectionHistoryGroup>,
    ) -> Result<SourceCollectionHistoryExecution, SourceWriterError> {
        if !group.within_observation_window()
            || !group
                .owner
                .ptr_eq(&Arc::downgrade(&self.reference_liveness))
        {
            return Err(SourceWriterError::StaleFence);
        }
        let current = self.current_revision()?;
        if current != group.revision {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(SourceCollectionHistoryExecution {
            revision: current.revision(),
            group,
            next_batch: 0,
            _thread: PhantomData,
        })
    }

    /// Commit only the next immutable collection batch after full original-owner comparison.
    /// Uses a fresh owner-issued write revision, never a caller-bumped SourceRevision.
    pub fn commit_collection_history_next(
        &mut self,
        execution: &mut SourceCollectionHistoryExecution,
        profile: &WritableProfileGuard,
    ) -> Result<SourceCurationCommitReceipt, SourceWriterError> {
        let group = &execution.group;
        if !group
            .owner
            .ptr_eq(&Arc::downgrade(&self.reference_liveness))
        {
            return Err(SourceWriterError::StaleFence);
        }
        let current = self.current_revision_for_command()?;
        let original = &group.revision;
        if current.source_id() != original.source_id()
            || current.root_identity() != original.root_identity()
            || current.root_binding_identity() != original.root_binding_identity()
            || current.database_identity() != original.database_identity()
            || current.lifecycle_generation() != original.lifecycle_generation()
            || current.lease_epoch() != original.lease_epoch()
            || current.revision() != execution.revision
        {
            return Err(SourceWriterError::StaleFence);
        }
        let changes =
            group
                .batches()
                .nth(execution.next_batch)
                .ok_or(SourceWriterError::InvalidRequest(
                    "collection group batch exhausted",
                ))?;
        let updates: Vec<_> = changes
            .iter()
            .map(|change| {
                if change.after {
                    SourceCurationUpdate::AddCollection {
                        path: change.path.clone(),
                        collection: group.collection,
                    }
                } else {
                    SourceCurationUpdate::RemoveCollection {
                        path: change.path.clone(),
                        collection: group.collection,
                    }
                }
            })
            .collect();
        for desired in [false, true] {
            let expected: Vec<_> = changes.iter().filter(|c| c.after == desired).collect();
            if expected.is_empty() {
                continue;
            }
            let paths: Vec<_> = expected.iter().map(|c| c.path.clone()).collect();
            let observed = self.capture_collection_history_selection(
                &current,
                group.collection,
                desired,
                &paths,
            )?;
            if observed.directory_generation != group.directory_generation
                || observed.changes.len() != expected.len()
                || !observed.changes.iter().zip(expected).all(|(a, b)| a == b)
            {
                return Err(SourceWriterError::StaleFence);
            }
        }
        let receipt = self.commit_catalog_curation_command(&current, profile, &updates)?;
        // Receipt is real committed state. Do not advance provenance using predictions.
        execution.revision = receipt.source_revision();
        execution.next_batch += 1;
        Ok(receipt)
    }
}
