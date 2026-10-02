//! Retained marker durability and bounded cached projection patching.
use super::*;

pub(super) fn retained(
    submission: &CatalogReplaySubmission,
    reason: CatalogReplayError,
) -> CatalogReplayOutcome {
    CatalogReplayOutcome::Retained {
        reason,
        batch: submission.batch.clone(),
    }
}
pub(super) fn audit(
    source: &mut RegisteredSource,
    submission: &CatalogReplaySubmission,
    profile: &WritableProfileGuard,
    admission: &Arc<Mutex<AdmissionState>>,
    reason: CatalogReplayError,
) -> CatalogReplayOutcome {
    let source_id = submission.batch.batch().provenance().source_id();
    let marker = source
        .replay
        .as_mut()
        .unwrap()
        .require_audit(&submission.batch);
    source.watcher_schedule.queue_replay_audit();
    let Some(marker) = marker else {
        return retained(submission, CatalogReplayError::CommitUncertain);
    };
    if let Some(token) = begin_source_attempt(admission, source_id, source.lifecycle_generation) {
        source.attempt_token = Some(token.clone());
        let writer = CatalogScanWriter {
            profile_guard: profile,
            database: &source.database,
            cancel: None,
            owner: Some(&source.database.writer),
            lifecycle: Some(CatalogScanLifecycle {
                admission,
                source_id,
                generation: source.lifecycle_generation,
                token: &token,
            }),
        };
        if let Ok(_guard) = writer.try_lock(ScanWritePhase::Manifest)
            && let Ok(fence) = source.database.writer.current_scan_fence()
        {
            if source
                .database
                .writer
                .retain_watcher_replay_uncertainty(&fence, &marker)
                .is_err()
            {
                return retained(submission, CatalogReplayError::CommitUncertain);
            }
        } else {
            return retained(submission, CatalogReplayError::OwnerUnavailable);
        }
    } else {
        return retained(submission, CatalogReplayError::StaleLifecycle);
    }
    CatalogReplayOutcome::AuditRequired { reason }
}

pub(super) fn patch_snapshot(
    source: &mut RegisteredSource,
    expected: u64,
    delta: &ManifestCommitResult,
) {
    let Some(snapshot) = source
        .snapshot
        .as_mut()
        .filter(|s| s.committed_revision == expected)
    else {
        return;
    };
    for (path, entry) in &delta.touched_path_changes {
        match (
            snapshot
                .manifest
                .binary_search_by(|e| e.relative_path.cmp(path)),
            entry,
        ) {
            (Ok(i), Some(entry)) => snapshot.manifest[i] = entry.clone(),
            (Err(i), Some(entry)) => snapshot.manifest.insert(i, entry.clone()),
            (Ok(i), None) => {
                snapshot.manifest.remove(i);
            }
            (Err(_), None) => {}
        }
        snapshot.wav_metadata.remove(path);
        // The scanner preserves source-owned curation for the same path. Only display metadata
        // derived from changed audio bytes is invalidated; replay cannot erase curation facts.
    }
    snapshot.committed_revision = delta.revision;
}
