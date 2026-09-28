use std::sync::atomic::{AtomicBool, Ordering};

use super::scan::{ScanContext, ScanError};
use super::scan_capability::SourceRootCapability;
use super::scan_diff::{absent_missing_candidates, mark_missing};
use super::scan_index::reconcile_index_entries;
use super::scan_writer::{ScanWritePhase, ScanWriter};
use crate::sample_sources::SourceDatabase;
use crate::sample_sources::db::META_LAST_SCAN_COMPLETED_AT;
use crate::sample_sources::db::SourceManifestEntry;

const MISSING_BATCH_SIZE: usize = 64;

pub(super) fn db_sync_phase(
    db: &SourceDatabase,
    source_root: &SourceRootCapability,
    context: &mut ScanContext,
    cancel: Option<&AtomicBool>,
    writer: &impl ScanWriter,
) -> Result<(u64, Vec<SourceManifestEntry>), ScanError> {
    // Partial traversal can commit observed additions, but it cannot establish
    // absence anywhere in the root. Delay every removal until a complete retry.
    if !context.has_uncertain_prefixes() {
        let mut existing = std::mem::take(&mut context.existing)
            .into_values()
            .filter(|entry| !context.preserves_missing_row(&entry.relative_path))
            .collect::<Vec<_>>()
            .into_iter();
        loop {
            if cancel_requested(cancel) {
                return Err(ScanError::Canceled);
            }
            let chunk = existing
                .by_ref()
                .take(MISSING_BATCH_SIZE)
                .collect::<Vec<_>>();
            if chunk.is_empty() {
                break;
            }
            let chunk = absent_missing_candidates(db.root(), context.traversal_policy(), chunk);
            if chunk.is_empty() {
                continue;
            }
            source_root.ensure_current_generation()?;
            let _writer = writer.try_lock(ScanWritePhase::Manifest)?;
            if cancel_requested(cancel) {
                return Err(ScanError::Canceled);
            }
            let mut batch = db.write_batch()?;
            context.ensure_rename_candidate_generation(&mut batch)?;
            mark_missing(db, &mut batch, chunk, &mut context.stats)?;
            if cancel_requested(cancel) {
                return Err(ScanError::Canceled);
            }
            context.commit_batch(db, batch, writer)?;
        }
    }

    source_root.ensure_current_generation()?;
    reconcile_index_entries(db, source_root, context, writer)?;
    if cancel_requested(cancel) {
        return Err(ScanError::Canceled);
    }
    source_root.ensure_current_generation()?;
    // An unreadable subtree is not a completed source scan. Keep its prior
    // manifest rows and completion metadata intact so the existing retry/audit
    // owner will revisit it instead of treating the partial traversal as
    // authoritative.
    if context.has_uncertain_prefixes() {
        return Ok(context.latest_committed_snapshot());
    }
    Ok(context.latest_committed_snapshot())
}

pub(super) fn complete_scan_generation(
    db: &SourceDatabase,
    source_root: &SourceRootCapability,
    context: &mut ScanContext,
    cancel: Option<&AtomicBool>,
    writer: &impl ScanWriter,
) -> Result<(u64, Vec<SourceManifestEntry>), ScanError> {
    if context.has_uncertain_prefixes() {
        return Ok(context.latest_committed_snapshot());
    }
    source_root.ensure_current_generation()?;
    let _writer = writer.try_lock(ScanWritePhase::Manifest)?;
    if cancel_requested(cancel) {
        return Err(ScanError::Canceled);
    }
    let mut batch = db.write_batch()?;
    context.ensure_rename_candidate_generation(&mut batch)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    batch.set_metadata(META_LAST_SCAN_COMPLETED_AT, &timestamp)?;
    if cancel_requested(cancel) {
        return Err(ScanError::Canceled);
    }
    if context.mode == super::scan::ScanMode::Targeted {
        let index_projection_is_current = !context.stats.committed_source_index_delta.is_empty()
            && context.stats.committed_source_index_delta.revision
                == context.latest_committed_snapshot().0;
        if index_projection_is_current {
            // Keep the index-bearing source revision contiguous. The index facts were already
            // committed through the bounded source write above; completion metadata is auxiliary
            // and must not create an unrepresentable intermediate browser revision.
            batch.commit_auxiliary_state()?;
        } else {
            context.commit_batch(db, batch, writer)?;
        }
        Ok(context.latest_committed_snapshot())
    } else {
        Ok(batch.commit_with_manifest_snapshot()?)
    }
}

fn cancel_requested(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}
