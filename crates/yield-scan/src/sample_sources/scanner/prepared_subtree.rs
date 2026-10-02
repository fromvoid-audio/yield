//! Complete bounded subtree observation prepared before one owner replay transaction.
use super::{
    prepared_exact::{self, canceled, invalid, owner_error},
    scan::ScanError,
    scan_capability::SourceRootCapability,
    scan_writer::{ScanWritePhase, ScanWriter},
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use yield_library::sample_sources::{
    SourceScanFence, SourceWriter,
    db::{ManifestCommitResult, SubtreeScanDatabaseFacts},
    reconciliation::RootRelativePath,
};
#[cfg(test)]
mod tests;
mod tree;

/// Maximum directory and regular entries visited, including excluded entries and lookahead.
pub const SUBTREE_SCAN_MAX_ENTRIES: usize = 128;
/// Maximum elapsed preparation/commit admission; this is a cooperative work deadline.
pub const SUBTREE_SCAN_MAX_ELAPSED: Duration = Duration::from_secs(5);

/// Held complete observation. Its private state binds every absence/directory replacement to
/// bounded retained facts and a fully enumerated namespace beneath the original owner.
pub struct PreparedSubtreeScan<'a> {
    owner: &'a SourceWriter,
    fence: SourceScanFence,
    facts: SubtreeScanDatabaseFacts,
    root: SourceRootCapability,
    parents: Vec<prepared_exact::parents::ParentDirectory>,
    tree: tree::HeldTree,
    files: prepared_exact::PreparedExactScan<'a>,
    absent: Vec<PathBuf>,
    missing_audio: Vec<PathBuf>,
    removed_index: Vec<PathBuf>,
    started: Instant,
}

/// Prepare complete non-overlapping, non-root subtrees and optional disjoint exact entries.
/// Traversal/hashing finishes outside SQL and mutation admission. Missing, uncertain, oversized,
/// incomplete or replaced targets fail the entire observation; callers must retain replay raw
/// and widen to a correlated audit, rather than acknowledging a partially inspected subtree.
pub fn prepare_subtree_scan<'a>(
    owner: &'a SourceWriter,
    subtrees: &[RootRelativePath],
    exact: &[RootRelativePath],
    cancel: Option<&AtomicBool>,
) -> Result<PreparedSubtreeScan<'a>, ScanError> {
    let started = Instant::now();
    check(started, cancel)?;
    let fence = owner.current_scan_fence().map_err(owner_error)?;
    let db = owner.catalog_database();
    let root = SourceRootCapability::open(db.root())?;
    if root.generation().as_bytes() != fence.root_identity().as_bytes() {
        return Err(invalid(
            db.root(),
            "subtree root differs from retained owner",
        ));
    }
    let facts = db.subtree_scan_database_facts_at_revision(subtrees, fence.revision())?;
    // Ancestors outside the selected scopes must remain the previously audited namespace.
    let parents = prepared_exact::parents::hold_parents(owner, subtrees, &root, fence.revision())?;
    if exact.len() > 64
        || exact.iter().any(|p| {
            subtrees
                .iter()
                .any(|s| p.as_path().starts_with(s.as_path()))
        })
    {
        return Err(invalid(db.root(), "overlapping subtree/exact scopes"));
    }
    let exact_parents = if exact.is_empty() {
        Vec::new()
    } else {
        prepared_exact::parents::hold_parents(owner, exact, &root, fence.revision())?
    };
    let mut parents = parents;
    parents.extend(exact_parents);
    let tree = tree::walk(&root, subtrees, facts.policy(), started, cancel)?;
    for prior in facts.directories() {
        if let Some(current) = tree
            .directories
            .iter()
            .find(|d| d.entry.relative_path == prior.relative_path)
        {
            if current.entry.directory_identity != prior.directory_identity {
                return Err(invalid(
                    &prior.relative_path,
                    "known subtree directory was replaced",
                ));
            }
        }
    }
    let mut paths = tree.files.clone();
    paths.extend_from_slice(exact);
    paths.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    paths.dedup();
    if paths.len() > prepared_exact::EXACT_SCAN_MAX_ENTRIES {
        return Err(invalid(db.root(), "subtree file count limit exceeded"));
    }
    let files = prepared_exact::prepare_exact_entries(owner, &paths, cancel, false)?;
    if files.expected_revision() != fence.revision() {
        return Err(invalid(
            db.root(),
            "subtree source revision changed during preparation",
        ));
    }
    let present = paths
        .iter()
        .map(|p| p.as_path())
        .chain(
            tree.directories
                .iter()
                .map(|d| d.entry.relative_path.as_path()),
        )
        .collect::<BTreeSet<_>>();
    let file_paths = paths.iter().map(|p| p.as_path()).collect::<BTreeSet<_>>();
    let missing_audio = facts
        .audio_paths()
        .iter()
        .filter(|p| !file_paths.contains(p.as_path()))
        .cloned()
        .collect();
    let removed_index = facts
        .index_paths()
        .iter()
        .filter(|p| !file_paths.contains(p.as_path()))
        .cloned()
        .collect();
    let absent = facts
        .audio_paths()
        .iter()
        .chain(facts.index_paths())
        .chain(facts.directories().iter().map(|d| &d.relative_path))
        .filter(|p| !present.contains(p.as_path()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let prepared = PreparedSubtreeScan {
        owner,
        fence,
        facts,
        root,
        parents,
        tree,
        files,
        absent,
        missing_audio,
        removed_index,
        started,
    };
    prepared.revalidate(cancel, true)?;
    Ok(prepared)
}
impl PreparedSubtreeScan<'_> {
    /// Exact source revision observed before traversal.
    pub fn expected_revision(&self) -> u64 {
        self.fence.revision()
    }
    fn revalidate(&self, cancel: Option<&AtomicBool>, membership: bool) -> Result<(), ScanError> {
        check(self.started, cancel)?;
        self.owner
            .validate_scan_fence(&self.fence)
            .map_err(owner_error)?;
        self.root.ensure_current_generation()?;
        prepared_exact::parents::revalidate(&self.parents, &self.root, cancel)?;
        self.tree
            .revalidate(&self.root, self.started, cancel, membership)?;
        self.files.revalidate(cancel)?;
        for path in &self.absent {
            check(self.started, cancel)?;
            if self.root.entry_exists_nofollow(path)? {
                return Err(invalid(path, "prepared subtree absence changed"));
            }
        }
        self.root.ensure_current_generation()?;
        Ok(())
    }
    /// Commit the complete bounded audio/index/absence/directory delta through the caller's
    /// final owner hook. A replay writer attaches its separately admitted checkpoint there.
    pub fn commit(
        self,
        writer: &impl ScanWriter,
        cancel: Option<&AtomicBool>,
    ) -> Result<ManifestCommitResult, ScanError> {
        self.revalidate(cancel, true)?; // Enumeration is finished before mutation admission.
        let _guard = writer.try_lock(ScanWritePhase::Manifest)?;
        self.revalidate(cancel, false)?; // Only held metadata/namespace checks; no traversal/hash.
        let mut batch = self.owner.catalog_database().write_batch()?;
        if batch.get_revision()? != self.fence.revision()
            || batch.get_source_index_revision()? != self.facts.index_revision()
        {
            return Err(invalid(
                self.owner.catalog_database().root(),
                "prepared subtree revisions changed",
            ));
        }
        self.files.apply(&mut batch, cancel)?;
        for path in &self.missing_audio {
            check(self.started, cancel)?;
            batch.set_missing(path, true)?;
        }
        for path in &self.removed_index {
            check(self.started, cancel)?;
            batch.remove_source_index_entry(path)?;
        }
        let entries = self
            .tree
            .directories
            .iter()
            .map(|d| d.entry.clone())
            .collect::<Vec<_>>();
        batch.apply_prepared_subtree_directory_truth(&self.facts, &entries)?;
        check(self.started, cancel)?;
        writer.commit_bounded_manifest_batch(batch, self.fence.revision())
    }
}
pub(super) fn check(started: Instant, cancel: Option<&AtomicBool>) -> Result<(), ScanError> {
    canceled(cancel)?;
    if started.elapsed() > SUBTREE_SCAN_MAX_ELAPSED {
        return Err(invalid(
            std::path::Path::new(""),
            "subtree elapsed budget exceeded",
        ));
    }
    Ok(())
}
