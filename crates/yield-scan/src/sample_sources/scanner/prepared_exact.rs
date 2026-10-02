//! A held, bounded exact scan prepared before one source-owner transaction.
use super::{
    scan::ScanError,
    scan_capability::{SourcePathBinding, SourceRootCapability},
    scan_fs::{FileFacts, compute_content_hash_with_reader, read_facts_from_open_file},
    scan_index::{index_entry_from_file_facts, non_unicode_index_entry},
    scan_writer::{ScanWritePhase, ScanWriter},
};

mod enrollment;
mod parents;

#[cfg(test)]
mod tests;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use yield_library::sample_sources::db::ManifestCommitResult;
use yield_library::sample_sources::reconciliation::RootRelativePath;
use yield_library::sample_sources::{
    SourceEntryFileType, SourceIndexEntry, SourceScanFence, SourceWriter, SourceWriterError,
    classify_source_entry_with_policy,
};

/// Maximum exact entries retained by one prepared transaction.
pub const EXACT_SCAN_MAX_ENTRIES: usize = 64;
/// Maximum aggregate locator bytes; every locator is also capped at 1024 bytes.
pub const EXACT_SCAN_MAX_PATH_BYTES: usize = 32 * 1024;
/// Maximum aggregate audio bytes hashed during one preparation.
pub const EXACT_SCAN_MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

struct AudioFile {
    facts: FileFacts,
    file: File,
    hash: String,
}

struct IndexFile {
    facts: FileFacts,
    file: File,
    entry: SourceIndexEntry,
}

/// Private prepared facts and held files, bound to the retained owner that supplied the DB.
///
/// This is scan observation, not filesystem-effect or watcher-continuity authority. Commit uses
/// the existing ScanWriter hook, which a catalog replay writer can use to atomically attach its
/// separately admitted checkpoint. Missing/subtree/uncertain targets require conservative audit.
pub struct PreparedExactScan<'a> {
    owner: &'a SourceWriter,
    fence: SourceScanFence,
    index_revision: u64,
    root: SourceRootCapability,
    audio: Vec<AudioFile>,
    index: Vec<IndexFile>,
    tracked_paths: Vec<std::path::PathBuf>,
    parents: Vec<parents::ParentDirectory>,
}

/// Prepare at most 64 exact regular entries without holding a mutation guard or SQL transaction.
///
/// File reads/hashing finish here. Duplicate, missing, directory, link, oversized or changing
/// targets fail the whole preparation; no source mutation has occurred. Unicode and raw paths
/// remain exact. The returned value cannot be retargeted or outlive its original source owner.
pub fn prepare_exact_scan<'a>(
    owner: &'a SourceWriter,
    paths: &[RootRelativePath],
    cancel: Option<&AtomicBool>,
) -> Result<PreparedExactScan<'a>, ScanError> {
    canceled(cancel)?;
    let db = owner.catalog_database();
    let fail = || invalid(db.root(), "exact scan input exceeds bounds or is ambiguous");
    if paths.is_empty() || paths.len() > EXACT_SCAN_MAX_ENTRIES {
        return Err(fail());
    }
    let mut bytes = 0usize;
    let mut unique = BTreeSet::new();
    for path in paths {
        let len = path.as_path().as_os_str().as_encoded_bytes().len();
        bytes = bytes.saturating_add(len);
        if len > 1024
            || bytes > EXACT_SCAN_MAX_PATH_BYTES
            || path.as_path().components().count() > 32
            || !unique.insert(path.as_path())
        {
            return Err(fail());
        }
    }
    let fence = owner.current_scan_fence().map_err(owner_error)?;
    let root = SourceRootCapability::open(db.root())?;
    if root.generation().as_bytes() != fence.root_identity().as_bytes() {
        return Err(invalid(
            db.root(),
            "source root differs from retained owner",
        ));
    }
    // Only scalar/existence facts are needed: preserve curation in SQL, never hydrate tags.
    let observed = db.exact_scan_database_facts_at_revision(paths, fence.revision())?;
    let parents = parents::hold_parents(owner, paths, &root, fence.revision())?;
    let policy = observed.policy;

    let mut prepared = PreparedExactScan {
        owner,
        fence,
        index_revision: observed.index_revision,
        root,
        audio: Vec::new(),
        index: Vec::new(),
        tracked_paths: observed.tracked_paths,
        parents,
    };
    let mut hash_bytes = 0u64;
    for path in paths {
        canceled(cancel)?;
        let path = path.as_path();
        let file = prepared
            .root
            .open_regular_file(path)?
            .ok_or_else(|| invalid(path, "exact regular entry unavailable"))?;
        let facts = read_facts_from_open_file(db.root(), &db.root().join(path), &file)?;
        if facts.file_identity.as_deref().is_none_or(str::is_empty) {
            return Err(invalid(path, "file identity unavailable"));
        }
        let classification =
            classify_source_entry_with_policy(path, SourceEntryFileType::File, policy);
        if path.to_str().is_some() && classification.indexes_audio() {
            hash_bytes = hash_bytes.saturating_add(facts.size);
            if hash_bytes > EXACT_SCAN_MAX_HASH_BYTES {
                return Err(invalid(path, "exact scan hash byte limit exceeded"));
            }
            let mut file = file;
            // A growing file must not expand the admitted content budget. Hash only the exact
            // observed byte count, then reject short reads or any changed content snapshot.
            let mut bounded = file.by_ref().take(facts.size);
            let hash =
                compute_content_hash_with_reader(&db.root().join(path), &mut bounded, cancel)?;
            if bounded.limit() != 0 {
                return Err(invalid(path, "exact entry shortened during hashing"));
            }
            let after = read_facts_from_open_file(db.root(), &db.root().join(path), &file)?;
            if !facts.same_content_snapshot(&after) {
                return Err(invalid(path, "exact entry changed during hashing"));
            }
            prepared.audio.push(AudioFile { facts, file, hash });
        } else {
            let entry = if path.to_str().is_none() {
                non_unicode_index_entry(
                    path.to_path_buf(),
                    Some(facts.size),
                    Some(facts.modified_ns),
                    facts.file_identity.clone(),
                )
            } else {
                classification
                    .file_classification()
                    .filter(|_| !classification.has_supported_audio())
                    .and_then(|class| {
                        index_entry_from_file_facts(
                            path.to_path_buf(),
                            class,
                            facts.size,
                            facts.modified_ns,
                            facts.file_identity.clone(),
                        )
                    })
                    .ok_or_else(|| invalid(path, "exact classification unavailable"))?
            };
            prepared.index.push(IndexFile { facts, file, entry });
        }
    }
    prepared.revalidate(cancel)?;
    Ok(prepared)
}

impl PreparedExactScan<'_> {
    /// Exact generic source revision captured before file preparation.
    pub fn expected_revision(&self) -> u64 {
        self.fence.revision()
    }
    /// Number of bounded entries retained, including index-only facts.
    pub fn entry_count(&self) -> usize {
        self.audio.len() + self.index.len()
    }

    fn revalidate(&self, cancel: Option<&AtomicBool>) -> Result<(), ScanError> {
        canceled(cancel)?;
        self.owner
            .validate_scan_fence(&self.fence)
            .map_err(owner_error)?;
        let db = self.owner.catalog_database();
        self.root.ensure_current_generation()?;
        parents::revalidate(&self.parents, &self.root, cancel)?;
        for (facts, file) in self
            .audio
            .iter()
            .map(|a| (&a.facts, &a.file))
            .chain(self.index.iter().map(|i| (&i.facts, &i.file)))
        {
            canceled(cancel)?;
            let current =
                read_facts_from_open_file(db.root(), &db.root().join(&facts.relative), file)?;
            if !facts.same_content_snapshot(&current)
                || self.root.path_binding(&facts.relative, file)? != SourcePathBinding::Matches
            {
                return Err(invalid(&facts.relative, "prepared exact entry changed"));
            }
        }
        self.root.ensure_current_generation()?;
        self.owner
            .validate_scan_fence(&self.fence)
            .map_err(owner_error)?;
        Ok(())
    }

    /// Revalidate all held files, then apply audio and index facts in one revision-fenced batch.
    ///
    /// Filesystem preparation finishes before the supplied owner's mutation guard is acquired.
    /// The existing `commit_bounded_manifest_batch` hook owns the single transaction outcome.
    /// No last-scan/audit completion marker is advanced by this exact observation.
    pub fn commit(
        self,
        writer: &impl ScanWriter,
        cancel: Option<&AtomicBool>,
    ) -> Result<ManifestCommitResult, ScanError> {
        self.revalidate(cancel)?;
        let _guard = writer.try_lock(ScanWritePhase::Manifest)?;
        canceled(cancel)?;
        self.owner
            .validate_scan_fence(&self.fence)
            .map_err(owner_error)?;
        let db = self.owner.catalog_database();
        let mut batch = db.write_batch()?;
        let actual = batch.get_revision()?;
        if actual != self.fence.revision() {
            return Err(ScanError::StaleRevision {
                expected: self.fence.revision(),
                actual,
            });
        }
        if batch.get_source_index_revision()? != self.index_revision {
            return Err(invalid(db.root(), "prepared index revision changed"));
        }
        for audio in &self.audio {
            canceled(cancel)?;
            let path = &audio.facts.relative;
            batch.remove_source_index_entry(path)?;
            batch.upsert_file_with_hash(
                path,
                audio.facts.size,
                audio.facts.modified_ns,
                &audio.hash,
            )?;
            batch.set_file_identity(path, audio.facts.file_identity.as_deref())?;
        }
        for index in &self.index {
            if self.tracked_paths.contains(&index.entry.relative_path) {
                batch.set_missing(&index.entry.relative_path, true)?;
            }
            batch.upsert_source_index_entry(&index.entry)?;
        }
        canceled(cancel)?;
        writer.commit_bounded_manifest_batch(batch, self.fence.revision())
    }
}

fn canceled(cancel: Option<&AtomicBool>) -> Result<(), ScanError> {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        Err(ScanError::Canceled)
    } else {
        Ok(())
    }
}
fn invalid(path: &Path, message: &'static str) -> ScanError {
    ScanError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, message),
    }
}
fn owner_error(error: SourceWriterError) -> ScanError {
    match error {
        SourceWriterError::StaleRevision { expected, actual } => {
            ScanError::StaleRevision { expected, actual }
        }
        SourceWriterError::Database(e) => ScanError::Db(e),
        e => ScanError::SourceOwner(e.to_string()),
    }
}
