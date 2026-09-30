#![allow(clippy::type_complexity)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread;

use crate::sample_sources::SourceDatabase;
use crate::sample_sources::db::{SourceDbError, SourceManifestEntry};
use yield_library::sample_sources::reconciliation::{
    RootIdentity, SourceAuditCommit, SourceAuditRequest,
};

use super::super::scan_capability::SourceRootCapability;
use super::super::scan_db_sync::{complete_scan_generation, db_sync_phase};
use super::super::scan_fs::ensure_root_dir;
use super::super::scan_hash::ContentAuditBudget;
use super::super::scan_walk::walk_phase;
use super::super::scan_writer::{ScanWriter, UncoordinatedScanWriter};
use super::{CommittedScanBatch, ScanContext, ScanError, ScanStats};

const TARGETED_RENAME_CANDIDATE_LIMIT: usize = 256;

/// Scan strategy used when walking a source root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Reconcile a bounded set of watcher-reported paths.
    Targeted,
    /// Update the database with new/modified files and mark missing entries.
    /// Full hashing is staged for large files to keep quick scans responsive.
    Quick,
    /// Force a full rescan, pruning missing rows and pending renames without
    /// matching current destinations to rebuild state from disk.
    Hard,
}

/// Recursively scan the source root, syncing supported audio files into the database.
/// Returns counts of added/updated/removed rows.
pub fn scan_once(db: &SourceDatabase) -> Result<ScanStats, ScanError> {
    scan(db, ScanMode::Quick, None, None, None, true)
}

/// Rescan the entire source, pruning rows for files that no longer exist and
/// clearing pending rename rows that have no matching current destinations.
pub fn hard_rescan(db: &SourceDatabase) -> Result<ScanStats, ScanError> {
    scan(db, ScanMode::Hard, None, None, None, true)
}

/// Reconcile the full source manifest and verify a bounded rotating content batch.
pub fn audit_source(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
) -> Result<ScanStats, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    let mut stats = scan_with_writer_using_root(
        db,
        &root,
        &source_root,
        ScanMode::Quick,
        cancel,
        None,
        None,
        false,
        &UncoordinatedScanWriter,
    )?;
    let mut stats = merge_audit_verification(
        &mut stats,
        super::super::scan_hash::verify_content_batch_with_source_root(
            db,
            &source_root,
            cancel,
            ContentAuditBudget::entry_limited(max_hashes),
            now_epoch_seconds(),
            &UncoordinatedScanWriter,
        ),
    )?;
    finalize_pending_rename_completion(
        db,
        &mut stats,
        ScanMode::Quick,
        db.get_revision()?,
        &UncoordinatedScanWriter,
        Some(&source_root),
    )?;
    Ok(stats)
}

/// Reconcile a source and verify content under explicit time, byte, and entry ceilings.
pub fn audit_source_with_budget(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
) -> Result<ScanStats, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    let mut stats = scan_with_writer_using_root(
        db,
        &root,
        &source_root,
        ScanMode::Quick,
        cancel,
        None,
        None,
        false,
        &UncoordinatedScanWriter,
    )?;
    let mut stats = merge_audit_verification(
        &mut stats,
        super::super::scan_hash::verify_content_batch_with_source_root(
            db,
            &source_root,
            cancel,
            budget,
            now_epoch_seconds(),
            &UncoordinatedScanWriter,
        ),
    )?;
    finalize_pending_rename_completion(
        db,
        &mut stats,
        ScanMode::Quick,
        db.get_revision()?,
        &UncoordinatedScanWriter,
        Some(&source_root),
    )?;
    Ok(stats)
}

/// Reconcile a source audit and durably record manifest completion before content verification.
pub fn audit_source_and_record(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
    completed_at: i64,
) -> Result<ScanStats, ScanError> {
    audit_source_and_record_with_progress(db, cancel, max_hashes, completed_at, &mut |_, _| {})
}

/// Reconcile and durably record a resumable source audit while publishing checked-file progress.
pub fn audit_source_and_record_with_progress(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
) -> Result<ScanStats, ScanError> {
    audit_source_and_record_after_scan(
        db,
        cancel,
        ContentAuditBudget::entry_limited(max_hashes),
        completed_at,
        Some(on_progress),
        &UncoordinatedScanWriter,
        || {},
        || {},
    )
}

/// Reconcile and durably record manifest completion independently from a bounded content slice.
pub fn audit_source_and_record_with_budget_and_progress(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
) -> Result<ScanStats, ScanError> {
    audit_source_and_record_with_budget_and_progress_and_writer(
        db,
        cancel,
        budget,
        completed_at,
        on_progress,
        &UncoordinatedScanWriter,
    )
}

/// Run a bounded audit while acquiring the owning runtime's writer only for short mutations.
pub fn audit_source_and_record_with_budget_and_progress_and_writer(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    match audit_source_and_record_with_budget_and_progress_and_writer_with_request(
        db,
        cancel,
        budget,
        completed_at,
        on_progress,
        writer,
        None,
    )? {
        ManifestAuditOutcome::Complete {
            stats,
            content_incomplete,
            ..
        } => match content_incomplete {
            Some(error) => Err(ScanError::Incomplete {
                committed: Box::new(stats),
                error,
            }),
            None => Ok(stats),
        },
        ManifestAuditOutcome::Incomplete {
            committed, error, ..
        } => Err(ScanError::Incomplete {
            committed: Box::new(committed),
            error,
        }),
    }
}

/// Run a bounded audit with an optional request-bound authority for its complete manifest commit.
///
/// A `None` request preserves the public scanner behavior and returns an unbound completion
/// result. Only the native source-processing path supplies a request, allowing the resulting
/// commit to mint a clearing receipt for that exact retained boundary.
pub fn audit_source_and_record_with_budget_and_progress_and_writer_with_request(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
    writer: &impl ScanWriter,
    audit_request: Option<&SourceAuditRequest>,
) -> Result<ManifestAuditOutcome, ScanError> {
    audit_source_and_record_after_scan_outcome(
        db,
        cancel,
        budget,
        completed_at,
        Some(on_progress),
        writer,
        audit_request,
        || {},
        || {},
    )
}

/// Result of the catalog-owned manifest audit boundary.
///
/// This result is returned immediately after a complete source traversal, manifest reconciliation,
/// and directory-truth publication. Deferred content verification and pending-rename finalization
/// are intentionally outside this boundary.
#[derive(Debug)]
pub struct CatalogManifestAudit {
    /// Statistics and the exact manifest revision committed by the audit.
    pub stats: ScanStats,
    /// Opaque authority for the committed complete audit.
    pub audit_commit: SourceAuditCommit,
}

/// Run the catalog-owned authoritative manifest audit with bounded directory staging.
///
/// The source traversal and manifest checkpoints may commit partial progress, but the audit
/// receipt and completion metadata are written only when the traversal supplies complete directory
/// evidence. Directory rows are staged in revision-neutral bounded transactions and become
/// visible only in the final revision-advancing activation transaction.
pub fn audit_manifest_for_catalog_with_budget_and_progress_and_writer(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
    writer: &impl ScanWriter,
    audit_request: Option<&SourceAuditRequest>,
) -> Result<CatalogManifestAudit, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    let mut stats = match scan_with_writer_using_root_and_committed_batch(
        db,
        &root,
        &source_root,
        ScanMode::Quick,
        cancel,
        Some(on_progress),
        Some(completed_at),
        false,
        false,
        false,
        true,
        &mut |_| {},
        writer,
    ) {
        Ok(stats) => stats,
        Err(ScanError::Incomplete { committed, error }) => {
            return Err(ScanError::Incomplete { committed, error });
        }
        Err(error) => return Err(error),
    };

    let complete_directory_evidence = stats
        .source_tree_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.complete_directory_entries())
        .is_some();
    if !complete_directory_evidence {
        let error = stats
            .source_tree_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.diagnostics.first())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                String::from("complete directory evidence was not produced by the traversal")
            });
        return Err(ScanError::Incomplete {
            committed: Box::new(stats),
            error,
        });
    }

    let audit_commit = record_staged_manifest_audit_completion(
        db,
        &mut stats,
        completed_at,
        writer,
        &source_root,
        cancel,
        audit_request,
    )?;
    Ok(CatalogManifestAudit {
        stats,
        audit_commit,
    })
}

/// Outcome of a source manifest audit with bounded content verification.
///
/// Manifest traversal coverage is represented independently from resumable content
/// verification. Callers that own watcher recovery barriers must advance them only for the
/// `Complete` variant; a `Complete` outcome may still carry a content checkpoint pause.
#[derive(Debug)]
pub enum ManifestAuditOutcome {
    /// The authoritative manifest traversal and reconciliation completed.
    Complete {
        /// Statistics and committed manifest delta from the audit.
        stats: ScanStats,
        /// Content verification stopped after a durable checkpoint, if applicable.
        content_incomplete: Option<String>,
        /// Opaque authority from the complete manifest commit and held source-root capability.
        audit_commit: SourceAuditCommit,
    },
    /// Manifest traversal or reconciliation was uncertain after a committed checkpoint.
    Incomplete {
        /// Statistics and committed partial manifest delta to publish before retrying.
        committed: ScanStats,
        /// Diagnostic describing why authoritative coverage remains incomplete.
        error: String,
        /// Physical root identity held while the partial manifest commit was attempted.
        root_identity: String,
    },
}

/// Reconcile and durably record a source audit while preserving manifest coverage ownership.
pub fn audit_source_and_record_with_budget_and_progress_and_writer_outcome(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: &mut impl FnMut(usize, &Path),
    writer: &impl ScanWriter,
) -> Result<ManifestAuditOutcome, ScanError> {
    audit_source_and_record_after_scan_outcome(
        db,
        cancel,
        budget,
        completed_at,
        Some(on_progress),
        writer,
        None,
        || {},
        || {},
    )
}

fn audit_source_and_record_after_scan(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    writer: &impl ScanWriter,
    before_record: impl FnOnce(),
    after_scan: impl FnOnce(),
) -> Result<ScanStats, ScanError> {
    match audit_source_and_record_after_scan_outcome(
        db,
        cancel,
        budget,
        completed_at,
        on_progress,
        writer,
        None,
        before_record,
        after_scan,
    )? {
        ManifestAuditOutcome::Complete {
            stats,
            content_incomplete,
            ..
        } => match content_incomplete {
            Some(error) => Err(ScanError::Incomplete {
                committed: Box::new(stats),
                error,
            }),
            None => Ok(stats),
        },
        ManifestAuditOutcome::Incomplete {
            committed, error, ..
        } => Err(ScanError::Incomplete {
            committed: Box::new(committed),
            error,
        }),
    }
}

fn audit_source_and_record_after_scan_outcome(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    budget: ContentAuditBudget,
    completed_at: i64,
    on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    writer: &impl ScanWriter,
    audit_request: Option<&SourceAuditRequest>,
    before_record: impl FnOnce(),
    after_scan: impl FnOnce(),
) -> Result<ManifestAuditOutcome, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    let held_root_identity = source_root.generation().to_owned();
    let mut stats = match scan_with_writer_using_root_and_committed_batch(
        db,
        &root,
        &source_root,
        ScanMode::Quick,
        cancel,
        on_progress,
        Some(completed_at),
        false,
        true,
        false,
        false,
        &mut |_| {},
        writer,
    ) {
        Ok(stats) => stats,
        Err(ScanError::Incomplete { committed, error }) => {
            return Ok(ManifestAuditOutcome::Incomplete {
                committed: *committed,
                error,
                root_identity: held_root_identity.clone(),
            });
        }
        Err(error) => return Err(error),
    };
    if stats
        .source_tree_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.complete_directory_entries())
        .is_none()
    {
        let error = stats
            .source_tree_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.diagnostics.first())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                String::from("complete directory evidence was not produced by the traversal")
            });
        return Ok(ManifestAuditOutcome::Incomplete {
            committed: stats,
            error,
            root_identity: source_root.generation().to_owned(),
        });
    }
    before_record();
    let audit_commit = match record_manifest_audit_completion(
        db,
        &mut stats,
        completed_at,
        writer,
        &source_root,
        cancel,
        audit_request,
    ) {
        Ok(audit_commit) => audit_commit,
        Err(error) => {
            if stats.committed_delta.revision > 0 {
                return Ok(ManifestAuditOutcome::Incomplete {
                    committed: stats,
                    error: error.to_string(),
                    root_identity: held_root_identity,
                });
            }
            return Err(error);
        }
    };
    after_scan();
    let verification = super::super::scan_hash::verify_content_batch_with_source_root(
        db,
        &source_root,
        cancel,
        budget,
        completed_at,
        writer,
    );
    let mut content_incomplete = None;
    match verification {
        Ok(verified) => stats.merge_deferred_hashes(verified),
        Err(ScanError::Incomplete { committed, error }) => {
            stats.merge_deferred_hashes(*committed);
            content_incomplete = Some(error);
        }
        Err(error) if stats.committed_delta.revision > 0 => {
            tracing::warn!(
                %error,
                revision = stats.committed_delta.revision,
                "Retaining committed manifest repair before retrying failed content audit"
            );
            return Ok(ManifestAuditOutcome::Incomplete {
                committed: stats,
                error: error.to_string(),
                root_identity: held_root_identity,
            });
        }
        Err(error) => return Err(error),
    }
    if content_incomplete.is_some() {
        return Ok(ManifestAuditOutcome::Complete {
            stats,
            content_incomplete,
            audit_commit: audit_commit.clone(),
        });
    }
    finalize_pending_rename_completion(
        db,
        &mut stats,
        ScanMode::Quick,
        db.get_revision()?,
        writer,
        Some(&source_root),
    )?;
    Ok(ManifestAuditOutcome::Complete {
        stats,
        content_incomplete,
        audit_commit,
    })
}

fn record_manifest_audit_completion(
    db: &SourceDatabase,
    stats: &mut ScanStats,
    completed_at: i64,
    writer: &impl ScanWriter,
    source_root: &SourceRootCapability,
    cancel: Option<&AtomicBool>,
    audit_request: Option<&SourceAuditRequest>,
) -> Result<SourceAuditCommit, ScanError> {
    record_staged_manifest_audit_completion(
        db,
        stats,
        completed_at,
        writer,
        source_root,
        cancel,
        audit_request,
    )
}

fn record_staged_manifest_audit_completion(
    db: &SourceDatabase,
    stats: &mut ScanStats,
    completed_at: i64,
    writer: &impl ScanWriter,
    source_root: &SourceRootCapability,
    cancel: Option<&AtomicBool>,
    audit_request: Option<&SourceAuditRequest>,
) -> Result<SourceAuditCommit, ScanError> {
    let before = stats.manifest_before.clone();
    let (generation, result) = {
        let directory_entries = stats
            .source_tree_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.complete_directory_entries())
            .ok_or_else(|| ScanError::Incomplete {
                committed: Box::new(stats.clone()),
                error: String::from(
                    "complete directory evidence was not produced by the traversal",
                ),
            })?;
        if cancel_requested(cancel) {
            return Err(ScanError::Canceled);
        }
        source_root.ensure_current_generation()?;
        let _writer = writer.try_lock(super::super::scan_writer::ScanWritePhase::Manifest)?;
        let generation = writer.begin_directory_truth_generation(
            db,
            u64::try_from(directory_entries.len())
                .map_err(|_| ScanError::Db(SourceDbError::Unexpected))?,
        )?;
        let result = (|| {
            drop(_writer);
            for chunk in directory_entries
                .chunks(yield_library::sample_sources::db::SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
            {
                if cancel_requested(cancel) {
                    return Err(ScanError::Canceled);
                }
                source_root.ensure_current_generation()?;
                let _writer =
                    writer.try_lock(super::super::scan_writer::ScanWritePhase::Manifest)?;
                writer.stage_directory_truth_entries(db, generation, chunk)?;
            }

            if cancel_requested(cancel) {
                return Err(ScanError::Canceled);
            }
            source_root.ensure_current_generation()?;
            let _writer = writer.try_lock(super::super::scan_writer::ScanWritePhase::Manifest)?;
            let expected_revision = {
                let expected = stats.committed_delta.revision;
                let current_revision = db.get_revision()?;
                if current_revision == expected {
                    current_revision
                } else {
                    let (observed_revision, current_manifest) =
                        db.manifest_snapshot_with_revision()?;
                    if current_manifest != stats.manifest_after {
                        return Err(ScanError::StaleRevision {
                            expected,
                            actual: observed_revision,
                        });
                    }
                    observed_revision
                }
            };
            let batch = db.write_batch()?;
            if !batch.matches_revision(expected_revision)? {
                return Err(ScanError::StaleRevision {
                    expected: expected_revision,
                    actual: batch.get_revision()?,
                });
            }
            let root_identity =
                RootIdentity::from_bytes(source_root.generation().as_bytes().to_vec());
            let (audit_commit, committed_manifest) = writer.commit_scan_audit_completion(
                batch,
                generation,
                expected_revision,
                completed_at,
                root_identity,
                audit_request,
            )?;
            Ok((audit_commit, committed_manifest))
        })();
        (generation, result)
    };
    match result {
        Ok((audit_commit, committed_manifest)) => {
            super::super::manifest::publish_committed_delta(
                stats,
                before,
                (audit_commit.committed_source_revision(), committed_manifest),
            );
            Ok(audit_commit)
        }
        Err(error) => {
            match discard_staged_directory_generation(db, generation, writer, source_root) {
                Ok(_) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            }
        }
    }
}

fn discard_staged_directory_generation(
    db: &SourceDatabase,
    generation: u64,
    writer: &impl ScanWriter,
    source_root: &SourceRootCapability,
) -> Result<(), ScanError> {
    loop {
        source_root.ensure_current_generation()?;
        let _writer = writer.try_lock_cleanup()?;
        let cleanup = writer.discard_directory_truth_generation_batch(
            db,
            generation,
            yield_library::sample_sources::db::SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT,
        )?;
        drop(_writer);
        if !cleanup.more_work {
            return Ok(());
        }
    }
}

#[cfg(test)]
pub(crate) fn audit_source_and_record_with_post_scan_hook(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
    completed_at: i64,
    after_scan: impl FnOnce(),
) -> Result<ScanStats, ScanError> {
    audit_source_and_record_after_scan(
        db,
        cancel,
        ContentAuditBudget::entry_limited(max_hashes),
        completed_at,
        None,
        &UncoordinatedScanWriter,
        || {},
        after_scan,
    )
}

#[cfg(test)]
pub(crate) fn audit_source_and_record_with_post_scan_hook_outcome(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
    completed_at: i64,
    after_scan: impl FnOnce(),
) -> Result<ManifestAuditOutcome, ScanError> {
    audit_source_and_record_after_scan_outcome(
        db,
        cancel,
        ContentAuditBudget::entry_limited(max_hashes),
        completed_at,
        None,
        &UncoordinatedScanWriter,
        None,
        || {},
        after_scan,
    )
}

#[cfg(test)]
pub(crate) fn audit_source_and_record_with_pre_record_hook(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
    completed_at: i64,
    before_record: impl FnOnce(),
) -> Result<ScanStats, ScanError> {
    audit_source_and_record_after_scan(
        db,
        cancel,
        ContentAuditBudget::entry_limited(max_hashes),
        completed_at,
        None,
        &UncoordinatedScanWriter,
        before_record,
        || {},
    )
}

fn merge_audit_verification(
    stats: &mut ScanStats,
    verification: Result<ScanStats, ScanError>,
) -> Result<ScanStats, ScanError> {
    match verification {
        Ok(verified) => stats.merge_deferred_hashes(verified),
        Err(ScanError::Incomplete { committed, error }) => {
            stats.merge_deferred_hashes(*committed);
            return Err(ScanError::Incomplete {
                committed: Box::new(std::mem::take(stats)),
                error,
            });
        }
        Err(error) if stats.committed_delta.revision > 0 => {
            tracing::warn!(
                %error,
                revision = stats.committed_delta.revision,
                "Publishing committed manifest repair before retrying incomplete content audit"
            );
            return Err(ScanError::Incomplete {
                committed: Box::new(std::mem::take(stats)),
                error: error.to_string(),
            });
        }
        Err(error) => return Err(error),
    }
    Ok(std::mem::take(stats))
}

fn now_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn cancel_requested(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed))
}

/// Scan with a progress callback, optionally honoring a cancel flag.
pub fn scan_with_progress(
    db: &SourceDatabase,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: &mut impl FnMut(usize, &Path),
) -> Result<ScanStats, ScanError> {
    scan(db, mode, cancel, Some(on_progress), None, true)
}

/// Scan with progress while acquiring an owning runtime's writer guard only for bounded database
/// mutations.
pub fn scan_with_progress_and_writer(
    db: &SourceDatabase,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: &mut impl FnMut(usize, &Path),
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    scan_with_writer(db, mode, cancel, Some(on_progress), None, true, writer)
}

fn scan(
    db: &SourceDatabase,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    manifest_audit_started_at: Option<i64>,
    finalize_pending_renames: bool,
) -> Result<ScanStats, ScanError> {
    scan_with_writer(
        db,
        mode,
        cancel,
        on_progress,
        manifest_audit_started_at,
        finalize_pending_renames,
        &UncoordinatedScanWriter,
    )
}

/// Scan with progress and invoke a callback after every bounded manifest transaction commits.
///
/// The callback receives the exact revision, touched paths, and structured delta
/// from each traversal batch. It runs on the scanning worker before the next batch.
/// A concurrent revision change fails this bounded callback path as stale.
pub fn scan_with_progress_and_writer_and_committed_batch(
    db: &SourceDatabase,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: &mut impl FnMut(usize, &Path),
    on_committed_batch: &mut impl FnMut(CommittedScanBatch),
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    scan_with_writer_using_root_and_committed_batch(
        db,
        &root,
        &source_root,
        mode,
        cancel,
        Some(on_progress),
        None,
        true,
        true,
        true,
        true,
        on_committed_batch,
        writer,
    )
}

fn scan_with_writer(
    db: &SourceDatabase,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    manifest_audit_started_at: Option<i64>,
    finalize_pending_renames: bool,
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    let root = ensure_root_dir(db)?;
    let source_root = SourceRootCapability::open(&root)?;
    source_root.ensure_current_generation()?;
    scan_with_writer_using_root(
        db,
        &root,
        &source_root,
        mode,
        cancel,
        on_progress,
        manifest_audit_started_at,
        finalize_pending_renames,
        writer,
    )
}

fn scan_with_writer_using_root(
    db: &SourceDatabase,
    root: &Path,
    source_root: &SourceRootCapability,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    manifest_audit_started_at: Option<i64>,
    finalize_pending_renames: bool,
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    scan_with_writer_using_root_and_committed_batch(
        db,
        root,
        source_root,
        mode,
        cancel,
        on_progress,
        manifest_audit_started_at,
        finalize_pending_renames,
        true,
        true,
        false,
        &mut |_| {},
        writer,
    )
}

fn scan_with_writer_using_root_and_committed_batch(
    db: &SourceDatabase,
    root: &Path,
    source_root: &SourceRootCapability,
    mode: ScanMode,
    cancel: Option<&AtomicBool>,
    mut on_progress: Option<&mut dyn FnMut(usize, &Path)>,
    manifest_audit_started_at: Option<i64>,
    finalize_pending_renames: bool,
    reconcile_renames: bool,
    complete_generation: bool,
    exact_committed_batches: bool,
    on_committed_batch: &mut dyn FnMut(CommittedScanBatch),
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    debug_assert_ne!(mode, ScanMode::Targeted);
    let (manifest_revision, manifest_before) =
        super::super::manifest::capture_manifest_with_revision(db)?;
    source_root.ensure_current_generation()?;
    let traversal_policy = db.source_traversal_policy()?;
    let mut context = ScanContext::new(db, mode, manifest_revision, manifest_before.clone())?;
    context.exact_committed_batches = exact_committed_batches;
    context.set_traversal_policy(traversal_policy);
    if let Some(started_at) = manifest_audit_started_at {
        context.resume_manifest_audit(db, started_at)?;
        if let Some((checked, _expected)) = context.manifest_audit_progress()
            && checked > 0
            && let Some(on_progress) = on_progress.as_mut()
        {
            on_progress(checked, root);
        }
    }
    let result = walk_phase(
        db,
        root,
        source_root,
        traversal_policy,
        cancel,
        &mut on_progress,
        &mut context,
        on_committed_batch,
        writer,
    )
    .and_then(|()| db_sync_phase(db, source_root, &mut context, cancel, writer))
    .and_then(|committed_snapshot| {
        if reconcile_renames {
            reconcile_scan_renames(
                db,
                source_root,
                &mut context,
                &manifest_before,
                committed_snapshot,
                cancel,
                writer,
            )
        } else {
            Ok(committed_snapshot)
        }
    })
    .and_then(|committed_snapshot| {
        if finalize_pending_renames && !context.has_uncertain_prefixes() {
            finalize_pending_rename_completion(
                db,
                &mut context.stats,
                mode,
                committed_snapshot.0,
                writer,
                Some(source_root),
            )?;
        }
        Ok(committed_snapshot)
    })
    .and_then(|_| {
        if complete_generation {
            complete_scan_generation(db, &source_root, &mut context, cancel, writer)
        } else {
            Ok(context.latest_committed_snapshot())
        }
    });
    finish_scan_result(manifest_before, context, result)
}

pub(crate) fn reconcile_scan_renames(
    db: &SourceDatabase,
    source_root: &SourceRootCapability,
    context: &mut ScanContext,
    manifest_before: &[SourceManifestEntry],
    committed_snapshot: (u64, Vec<SourceManifestEntry>),
    cancel: Option<&AtomicBool>,
    writer: &impl ScanWriter,
) -> Result<(u64, Vec<SourceManifestEntry>), ScanError> {
    source_root.ensure_current_generation()?;
    // A partial traversal cannot safely consume pending rename state: a
    // retained source beneath an uncertain prefix may otherwise be claimed as
    // a move by an observed destination elsewhere. This also keeps hard
    // rescans from clearing pending metadata until the next complete pass.
    if context.has_uncertain_prefixes() {
        return Ok(committed_snapshot);
    }
    let current_candidates = context
        .stats
        .rename_candidate_paths
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let persisted_candidates = if context.mode == ScanMode::Targeted {
        let relevant_pending = relevant_pending_paths(
            db,
            manifest_before
                .iter()
                .map(|entry| entry.relative_path.clone()),
            &current_candidates,
        )?;
        let mut persisted =
            recent_unretained_candidates_for_pending_paths(db, relevant_pending.iter().cloned())?;
        for path in relevant_pending.iter().chain(current_candidates.iter()) {
            persisted.extend(db.list_retained_rename_destinations_for_pending_path(path)?);
        }
        persisted
    } else if context.mode == ScanMode::Hard {
        db.list_pending_rename_destinations()?
            .into_iter()
            .collect::<HashSet<_>>()
    } else {
        db.list_retained_rename_destinations()?
            .into_iter()
            .collect::<HashSet<_>>()
    };
    let mut candidates = current_candidates.clone();
    candidates.extend(persisted_candidates.iter().cloned());
    let carried_candidates_need_revalidation = persisted_candidates
        .iter()
        .any(|path| !current_candidates.contains(path));
    context.stats.pending_renames_considered = context
        .stats
        .pending_renames_considered
        .saturating_add(candidates.len());
    let (renamed, rename_revision, recovery_manifest_rows, recovery_manifest_queries) =
        if carried_candidates_need_revalidation {
            let deferred = super::super::scan_hash::deep_hash_scan_with_source_root_and_writer(
                db,
                source_root,
                cancel,
                &candidates,
                super::super::scan_hash::DeferredHashScope::RenameCandidates,
                None,
                None,
                writer,
            )?;
            (
                deferred.renamed_samples,
                Some(deferred.committed_delta.revision),
                deferred.targeted_manifest_rows_read,
                deferred.targeted_manifest_query_count,
            )
        } else {
            let reconciliation = super::super::scan_hash::reconcile_hashed_rename_candidates_with_source_root_and_writer(
            db,
            source_root,
            &candidates,
            cancel,
            writer,
        )?;
            (
                reconciliation.renamed_samples,
                reconciliation.committed_revision,
                reconciliation.targeted_manifest_rows_read,
                reconciliation.targeted_manifest_query_count,
            )
        };
    if context.mode == ScanMode::Targeted {
        context.stats.targeted_manifest_rows_read = context
            .stats
            .targeted_manifest_rows_read
            .saturating_add(recovery_manifest_rows);
        context.stats.targeted_manifest_query_count = context
            .stats
            .targeted_manifest_query_count
            .saturating_add(recovery_manifest_queries);
    }
    source_root.ensure_current_generation()?;
    if renamed.is_empty() && context.mode != ScanMode::Hard {
        if rename_revision.is_some() && context.mode != ScanMode::Targeted {
            // A carried rename candidate may have required a bounded hash/revalidation write
            // even when no move was proven. Return that transaction's current snapshot so the
            // later audit receipt is fenced to the revision actually on disk.
            return Ok(db.manifest_snapshot_with_revision()?);
        }
        if context.mode == ScanMode::Targeted {
            let expected = rename_revision.unwrap_or(committed_snapshot.0);
            let actual = db.get_revision()?;
            if actual != expected {
                return Err(ScanError::StaleRevision { expected, actual });
            }
            if rename_revision.is_some() {
                context.refresh_targeted_manifest(db, std::iter::empty(), Some(expected))?;
                return Ok(context.latest_committed_snapshot());
            }
        }
        return Ok(committed_snapshot);
    }

    let original_paths = manifest_before
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect::<HashSet<_>>();
    for rename in &renamed {
        if current_candidates.contains(&rename.new_relative_path) {
            context.stats.added = context.stats.added.saturating_sub(1);
            context.stats.content_changed = context.stats.content_changed.saturating_sub(1);
            context
                .stats
                .changed_samples
                .retain(|sample| sample.relative_path != rename.new_relative_path);
        }
        if original_paths.contains(&rename.old_relative_path) {
            context.stats.missing = context.stats.missing.saturating_sub(1);
        }
    }
    context.stats.rename_candidate_paths.retain(|candidate| {
        !renamed
            .iter()
            .any(|rename| rename.new_relative_path == *candidate)
    });
    context.stats.updated += renamed.len();
    context.stats.renames_reconciled += renamed.len();
    if context.mode == ScanMode::Targeted {
        context.refresh_targeted_manifest(
            db,
            renamed
                .iter()
                .map(|rename| rename.new_relative_path.clone()),
            rename_revision,
        )?;
        let committed_snapshot = context.latest_committed_snapshot();
        context.stats.renamed_samples.extend(renamed);
        return Ok(committed_snapshot);
    }
    context.stats.renamed_samples.extend(renamed);
    Ok(db.manifest_snapshot_with_revision()?)
}

fn finalize_pending_rename_completion(
    db: &SourceDatabase,
    stats: &mut ScanStats,
    mode: ScanMode,
    expected_revision: u64,
    writer: &impl ScanWriter,
    source_root: Option<&SourceRootCapability>,
) -> Result<(), ScanError> {
    debug_assert_ne!(mode, ScanMode::Targeted);
    if let Some(source_root) = source_root {
        source_root.ensure_current_generation()?;
    }
    let _writer = writer.try_lock(super::super::scan_writer::ScanWritePhase::Manifest)?;
    let mut batch = db.write_batch()?;
    if !batch.matches_revision(expected_revision)? {
        return Err(ScanError::StaleRevision {
            expected: expected_revision,
            actual: batch.get_revision()?,
        });
    }
    if mode == ScanMode::Hard {
        batch.prune_invalid_retained_rename_destinations()?;
    }
    let report = batch.complete_pending_rename_authoritative_scan(mode == ScanMode::Hard)?;
    if mode == ScanMode::Hard && report.diagnostics.candidate_count == 0 {
        batch.clear_unretained_pending_rename_destinations()?;
    }
    if let Some(source_root) = source_root {
        source_root.ensure_current_generation()?;
    }
    writer.commit_pending_rename_scan_completion(batch, expected_revision)?;
    stats.pending_renames_pruned = stats
        .pending_renames_pruned
        .saturating_add(report.candidates_pruned);
    stats.pending_rename_diagnostics = Some(report.diagnostics.clone());
    tracing::debug!(
        authoritative_generation = report.diagnostics.authoritative_generation,
        candidates = report.diagnostics.candidate_count,
        oldest_generation = report.diagnostics.oldest_staged_generation,
        oldest_staged_at = report.diagnostics.oldest_staged_at,
        oldest_age_seconds = report.diagnostics.oldest_candidate_age_seconds,
        considered = stats.pending_renames_considered,
        pruned = report.candidates_pruned,
        "Completed bounded pending-rename retention"
    );
    Ok(())
}

pub(crate) fn finish_scan_result(
    manifest_before: Vec<yield_library::sample_sources::SourceManifestEntry>,
    mut context: ScanContext,
    result: Result<(u64, Vec<yield_library::sample_sources::SourceManifestEntry>), ScanError>,
) -> Result<ScanStats, ScanError> {
    match result {
        Ok(committed_snapshot) => {
            if context.mode == super::ScanMode::Targeted {
                super::super::manifest::publish_targeted_committed_delta(
                    &mut context.stats,
                    manifest_before,
                    committed_snapshot,
                );
            } else {
                super::super::manifest::publish_committed_delta(
                    &mut context.stats,
                    manifest_before,
                    committed_snapshot,
                );
            }
            if context.has_uncertain_prefixes() {
                let error = context.uncertainty_error();
                return Err(ScanError::Incomplete {
                    committed: Box::new(context.stats),
                    error,
                });
            }
            if context.manifest_audit_revalidation_pending() {
                return Err(ScanError::Incomplete {
                    committed: Box::new(context.stats),
                    error: String::from("manifest audit checkpoint revalidation remains due"),
                });
            }
            Ok(context.stats)
        }
        Err(error) => {
            if context.mode == super::ScanMode::Targeted
                && matches!(
                    error,
                    ScanError::StaleRevision { .. }
                        | ScanError::TargetedRenameCandidateOverflow { .. }
                )
            {
                return Err(error);
            }
            let Some(committed_revision) = context.last_committed_revision else {
                return Err(error);
            };
            let committed_snapshot = context.committed_snapshot(committed_revision);
            if context.mode == super::ScanMode::Targeted {
                super::super::manifest::publish_targeted_committed_delta(
                    &mut context.stats,
                    manifest_before,
                    committed_snapshot,
                );
            } else {
                super::super::manifest::publish_committed_delta(
                    &mut context.stats,
                    manifest_before,
                    committed_snapshot,
                );
            }
            Err(ScanError::Incomplete {
                committed: Box::new(context.stats),
                error: error.to_string(),
            })
        }
    }
}

/// Spawn a background thread that opens the source database and performs one scan.
/// Useful for fire-and-forget refreshes without blocking the UI thread.
pub fn scan_in_background(root: PathBuf) -> thread::JoinHandle<Result<ScanStats, ScanError>> {
    thread::spawn(move || {
        let db = SourceDatabase::open_for_scan(&root)?;
        let stats = scan_once(&db)?;
        complete_deferred_hashes(&db, stats)
    })
}

/// Complete deferred content hashing and proven pending-rename reconciliation
/// before publishing scan results to cache consumers.
///
/// Callers should use this from their existing background worker. Hashing runs
/// without a source write transaction; only the final backfill/reconciliation
/// uses a short write batch.
pub fn complete_deferred_hashes(
    db: &SourceDatabase,
    stats: ScanStats,
) -> Result<ScanStats, ScanError> {
    complete_deferred_hashes_with_cancel(db, stats, None)
}

/// Reconcile only proven rename destinations before publishing latency-sensitive results.
///
/// Unrelated large-file hash backfills remain deferred. A cold import with no
/// pending source rows returns immediately even though its new paths are tracked
/// as possible destinations for a following watcher batch.
pub fn complete_deferred_rename_candidates(
    db: &SourceDatabase,
    stats: ScanStats,
) -> Result<ScanStats, ScanError> {
    complete_deferred_rename_candidates_with_cancel(db, stats, None)
}

/// Reconcile only proven rename destinations while honoring the owning runtime's cancellation.
pub fn complete_deferred_rename_candidates_with_cancel(
    db: &SourceDatabase,
    stats: ScanStats,
    cancel: Option<&AtomicBool>,
) -> Result<ScanStats, ScanError> {
    complete_deferred_rename_candidates_with_cancel_and_writer(
        db,
        stats,
        cancel,
        &UncoordinatedScanWriter,
    )
}

/// Reconcile proven rename destinations while coordinating only the final database publication.
pub fn complete_deferred_rename_candidates_with_cancel_and_writer(
    db: &SourceDatabase,
    mut stats: ScanStats,
    cancel: Option<&AtomicBool>,
    writer: &impl ScanWriter,
) -> Result<ScanStats, ScanError> {
    if !db.has_pending_renames()? {
        return Ok(stats);
    }
    let rename_candidates = stats
        .rename_candidate_paths
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut rename_candidates = rename_candidates;
    extend_retained_rename_candidates(
        db,
        &mut rename_candidates,
        stats
            .manifest_before
            .iter()
            .map(|entry| entry.relative_path.clone()),
    )?;
    let deferred = super::super::scan_hash::deep_hash_scan_with_writer(
        db,
        cancel,
        &rename_candidates,
        super::super::scan_hash::DeferredHashScope::RenameCandidates,
        None,
        None,
        writer,
    )?;
    stats.merge_deferred_hashes(deferred);
    Ok(stats)
}

/// Complete deferred hashing while honoring the owning scan's cancellation flag.
pub fn complete_deferred_hashes_with_cancel(
    db: &SourceDatabase,
    mut stats: ScanStats,
    cancel: Option<&AtomicBool>,
) -> Result<ScanStats, ScanError> {
    if stats.hashes_pending == 0
        && stats.rename_candidate_paths.is_empty()
        && !db.has_pending_renames()?
    {
        return Ok(stats);
    }
    let rename_candidates = stats
        .rename_candidate_paths
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut rename_candidates = rename_candidates;
    if stats.hashes_pending == 0 {
        extend_retained_rename_candidates(
            db,
            &mut rename_candidates,
            stats
                .manifest_before
                .iter()
                .map(|entry| entry.relative_path.clone()),
        )?;
    }
    let scope = if stats.hashes_pending > 0 {
        super::super::scan_hash::DeferredHashScope::AllUnhashed
    } else {
        super::super::scan_hash::DeferredHashScope::RenameCandidates
    };
    let deferred =
        super::super::scan_hash::deep_hash_scan(db, cancel, &rename_candidates, scope, None, None)?;
    stats.merge_deferred_hashes(deferred);
    Ok(stats)
}

fn extend_retained_rename_candidates(
    db: &SourceDatabase,
    candidates: &mut HashSet<std::path::PathBuf>,
    pending_paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<(), ScanError> {
    let pending_paths = pending_paths.into_iter().collect::<HashSet<_>>();
    let has_pending_renames = db.has_pending_renames()?;
    let pending_paths = relevant_pending_paths(db, pending_paths, candidates)?;
    if has_pending_renames {
        candidates.extend(recent_unretained_candidates_for_pending_paths(
            db,
            pending_paths.iter().cloned(),
        )?);
    }

    let mut retained_match = false;
    for path in pending_paths {
        let retained = db.list_retained_rename_destinations_for_pending_path(&path)?;
        retained_match |= !retained.is_empty();
        candidates.extend(retained);
    }

    if has_pending_renames && !retained_match {
        let (paths, overflow) =
            db.list_retained_rename_destinations_bounded(TARGETED_RENAME_CANDIDATE_LIMIT)?;
        if overflow {
            return Err(ScanError::TargetedRenameCandidateOverflow {
                limit: TARGETED_RENAME_CANDIDATE_LIMIT,
            });
        }
        candidates.extend(paths);
    }
    Ok(())
}

fn recent_unretained_candidates_for_pending_paths(
    db: &SourceDatabase,
    pending_paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<HashSet<std::path::PathBuf>, ScanError> {
    let mut candidates = HashSet::new();
    for path in pending_paths.into_iter().collect::<HashSet<_>>() {
        let remaining = TARGETED_RENAME_CANDIDATE_LIMIT.saturating_sub(candidates.len());
        let (paths, overflow) =
            db.list_recent_unretained_rename_destinations_for_pending_path(&path, remaining)?;
        if overflow {
            return Err(ScanError::TargetedRenameCandidateOverflow {
                limit: TARGETED_RENAME_CANDIDATE_LIMIT,
            });
        }
        candidates.extend(paths);
    }
    Ok(candidates)
}

fn relevant_pending_paths(
    db: &SourceDatabase,
    pending_paths: impl IntoIterator<Item = std::path::PathBuf>,
    current_candidates: &HashSet<std::path::PathBuf>,
) -> Result<HashSet<std::path::PathBuf>, ScanError> {
    if !db.has_pending_renames()? {
        return Ok(HashSet::new());
    }
    let mut relevant = HashSet::new();
    for path in pending_paths.into_iter().collect::<HashSet<_>>() {
        if db.has_pending_rename_for_path(&path)? {
            relevant.insert(path);
        }
    }
    if relevant.len() > TARGETED_RENAME_CANDIDATE_LIMIT {
        return Err(ScanError::TargetedRenameCandidateOverflow {
            limit: TARGETED_RENAME_CANDIDATE_LIMIT,
        });
    }
    for destination in current_candidates {
        let (paths, overflow) = db.list_pending_rename_paths_for_destination(
            destination,
            TARGETED_RENAME_CANDIDATE_LIMIT,
        )?;
        if overflow {
            return Err(ScanError::TargetedRenameCandidateOverflow {
                limit: TARGETED_RENAME_CANDIDATE_LIMIT,
            });
        }
        relevant.extend(paths);
        if relevant.len() > TARGETED_RENAME_CANDIDATE_LIMIT {
            return Err(ScanError::TargetedRenameCandidateOverflow {
                limit: TARGETED_RENAME_CANDIDATE_LIMIT,
            });
        }
    }
    Ok(relevant)
}

/// Complete a bounded batch of pending deep-content hashes without launching an unowned worker.
///
/// Explicit scan workflows may use this bounded batch helper. Long-lived runtimes should instead
/// schedule [`complete_pending_deep_hash_for_path`] behind per-file durable work so failures cannot
/// starve later paths. Proven rename candidates are always reconciled even when the hash budget is
/// exhausted.
pub fn complete_pending_deep_hashes(
    db: &SourceDatabase,
    cancel: Option<&AtomicBool>,
    max_hashes: usize,
) -> Result<ScanStats, ScanError> {
    super::super::scan_hash::deep_hash_scan(
        db,
        cancel,
        &HashSet::new(),
        super::super::scan_hash::DeferredHashScope::AllUnhashed,
        Some(max_hashes),
        None,
    )
}

/// Complete the pending deep-content hash for one exact tracked source-relative path.
///
/// The caller owns scheduling, durable retry policy, cancellation, and resource budgets. Work for
/// other paths is deliberately excluded so one failure cannot abort or starve a path-ordered batch.
pub fn complete_pending_deep_hash_for_path(
    db: &SourceDatabase,
    relative_path: &std::path::Path,
    cancel: Option<&AtomicBool>,
) -> Result<ScanStats, ScanError> {
    super::super::scan_hash::deep_hash_scan(
        db,
        cancel,
        &HashSet::new(),
        super::super::scan_hash::DeferredHashScope::AllUnhashed,
        Some(1),
        Some(relative_path),
    )
}
