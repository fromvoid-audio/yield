//! Production owner admission, exact atomic application and durable retirement.
use super::recovery::{audit, patch_snapshot, retained};
use super::state::PendingReplay;
use super::writer::ReplayWriter;
use super::*;
use std::cell::{Cell, RefCell};
use yield_library::sample_sources::reconciliation::{
    AdapterDisposition, AdmissionOutcome, ReconciliationScopeKind, WatcherGeneration,
};
use yield_scan::sample_sources::scanner::{
    EXACT_SCAN_MAX_ENTRIES, prepare_exact_scan, prepare_subtree_scan,
};

pub(super) fn consume(
    submission: &CatalogReplaySubmission,
    profile: &WritableProfileGuard,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> CatalogReplayOutcome {
    let source_id = submission.batch.batch().provenance().source_id();
    let life = submission.lifecycle_generation;
    if submission.cancellation.load(Ordering::Acquire) {
        return retained(submission, CatalogReplayError::Canceled);
    }
    let Some(source) = sources
        .get_mut(source_id)
        .filter(|s| s.lifecycle_generation == life)
    else {
        return retained(submission, CatalogReplayError::StaleLifecycle);
    };
    if !publication_is_current(admission, source_id, life, None) {
        return retained(submission, CatalogReplayError::StaleLifecycle);
    }
    if profile.validate_current().is_err()
        || source.database.validate_profile_guard(profile).is_err()
        || source.publication_fence.validate().is_err()
    {
        return retained(submission, CatalogReplayError::OwnerUnavailable);
    }
    let Ok(fence) = source.database.writer.current_scan_fence() else {
        return retained(submission, CatalogReplayError::OwnerUnavailable);
    };
    let generation = WatcherGeneration::new(life); // Retained owner generation, never caller-derived.
    let receipt = match source.database.read_durable_replay_receipt(
        source_id,
        fence.root_identity(),
        generation,
        generation,
    ) {
        Ok(r) => r,
        Err(_) => return retained(submission, CatalogReplayError::OwnerUnavailable),
    };
    if source.replay.is_none() {
        let state = match ReplayState::new(
            source_id.clone(),
            fence.root_identity().clone(),
            receipt.as_ref().map(DurableReplayReceipt::prior),
            submission.permit.clone(),
        ) {
            Ok(s) => s,
            Err(e) => return retained(submission, e),
        };
        source.replay = Some(state);
    }
    let provenance = submission.batch.batch().provenance();
    let identity_matches = provenance.root_identity() == Some(fence.root_identity())
        && provenance.watcher_generation() == generation
        && receipt.as_ref().is_none_or(|r| {
            provenance.backend_stream_identity() == Some(r.prior().backend_stream_identity())
        });
    if !identity_matches {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::WrongIdentity,
        );
    }
    if source
        .replay
        .as_ref()
        .unwrap()
        .pending
        .as_ref()
        .is_some_and(|p| p.batch != submission.batch)
    {
        return retained(submission, CatalogReplayError::PendingDifferent);
    }
    if source
        .replay
        .as_ref()
        .unwrap()
        .audit_batch
        .as_ref()
        .is_some_and(|b| b != &submission.batch)
    {
        return retained(submission, CatalogReplayError::PendingDifferent);
    }
    if source.replay.as_ref().unwrap().needs_audit {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::GapOrOverflow,
        );
    }
    if submission.batch.continuous()
        && let Some(receipt) = receipt.as_ref()
        && provenance
            .capture_boundary()
            .first_sequence()
            .zip(provenance.capture_boundary().last_sequence())
            .is_some_and(|(first, last)| receipt.matches_interval(first, last))
    {
        let state = source.replay.as_mut().unwrap();
        if let Some(pending) = state.pending.as_mut() {
            if !pending.applied
                && state
                    .owner
                    .mark_applied(pending.dispatched.ticket())
                    .is_err()
            {
                return retained(submission, CatalogReplayError::CommitUncertain);
            }
            pending.applied = true;
            if state
                .owner
                .mark_replay_checkpointed(pending.dispatched.ticket(), receipt.prior())
                .is_err()
            {
                return retained(submission, CatalogReplayError::CommitUncertain);
            }
            state.pending = None;
        }
        source.replay = None;
        return CatalogReplayOutcome::PreviouslyCommitted {
            receipt: receipt.clone(),
        };
    }
    let Some(receipt) = receipt else {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::MissingPrior,
        );
    };
    if fence.revision() != submission.expected_revision {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::StaleRevision,
        );
    }
    let state = source.replay.as_mut().unwrap();
    if state.pending.is_none() {
        let admitted = match state.owner.admit_replay_with_durable_prior(
            submission.batch.batch().clone(),
            Some(receipt.prior()),
            submission.batch.continuous(),
        ) {
            Ok(a) => a,
            Err(_) => {
                return audit(
                    source,
                    submission,
                    profile,
                    admission,
                    CatalogReplayError::GapOrOverflow,
                );
            }
        };
        if admitted.admission().disposition() != AdapterDisposition::AdmittedWithContinuity {
            return audit(
                source,
                submission,
                profile,
                admission,
                CatalogReplayError::GapOrOverflow,
            );
        }
        let AdmissionOutcome::Accepted(ticket) = admitted.admission().outcome() else {
            return audit(
                source,
                submission,
                profile,
                admission,
                CatalogReplayError::GapOrOverflow,
            );
        };
        let Some(dispatched) = state
            .owner
            .dispatch_next()
            .filter(|d| d.ticket() == *ticket)
        else {
            return retained(submission, CatalogReplayError::CommitUncertain);
        };
        if state.owner.mark_dispatched(*ticket).is_err() {
            return retained(submission, CatalogReplayError::CommitUncertain);
        }
        state.pending = Some(PendingReplay {
            batch: submission.batch.clone(),
            dispatched,
            applied: false,
        });
    }
    let pending = state.pending.as_ref().unwrap();
    if pending
        .dispatched
        .normalized()
        .scopes()
        .iter()
        .any(|s| s.kind() == ReconciliationScopeKind::SourceAudit)
    {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::SubtreeAudit,
        );
    }
    let mut paths = pending
        .dispatched
        .normalized()
        .scopes()
        .iter()
        .filter_map(|s| s.path().cloned())
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    paths.dedup();
    if paths.is_empty() || paths.len() > EXACT_SCAN_MAX_ENTRIES {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::InvalidTarget,
        );
    }
    let subtrees = pending
        .dispatched
        .normalized()
        .scopes()
        .iter()
        .filter(|s| s.kind() == ReconciliationScopeKind::Subtree)
        .filter_map(|s| s.path().cloned())
        .collect::<Vec<_>>();
    let exact = pending
        .dispatched
        .normalized()
        .scopes()
        .iter()
        .filter(|s| s.kind() == ReconciliationScopeKind::ExactEntry)
        .filter_map(|s| s.path().cloned())
        .collect::<Vec<_>>();
    let Some(proof) = pending
        .dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .cloned()
    else {
        return audit(
            source,
            submission,
            profile,
            admission,
            CatalogReplayError::GapOrOverflow,
        );
    };
    let Some(token) = begin_source_attempt(admission, source_id, life) else {
        return retained(submission, CatalogReplayError::StaleLifecycle);
    };
    source.attempt_token = Some(token.clone());
    let base = CatalogScanWriter {
        profile_guard: profile,
        database: &source.database,
        cancel: Some(&submission.cancellation),
        owner: Some(&source.database.writer),
        lifecycle: Some(CatalogScanLifecycle {
            admission,
            source_id,
            generation: life,
            token: &token,
        }),
    };
    let writer = ReplayWriter {
        base,
        cancellation: &submission.cancellation,
        prior: receipt.prior(),
        proof: &proof,
        checkpoint: RefCell::new(None),
        commit_uncertain: Cell::new(false),
    };
    let result = if subtrees.is_empty() {
        prepare_exact_scan(
            &source.database.writer,
            &paths,
            Some(&submission.cancellation),
        )
        .and_then(|prepared| prepared.commit(&writer, Some(&submission.cancellation)))
    } else {
        prepare_subtree_scan(
            &source.database.writer,
            &subtrees,
            &exact,
            Some(&submission.cancellation),
        )
        .and_then(|prepared| prepared.commit(&writer, Some(&submission.cancellation)))
    };
    let delta = match result {
        Ok(delta) => delta,
        Err(_) if writer.commit_uncertain.get() => {
            // Do not poison the persisted receipt: retry its exact interval first.
            return retained(submission, CatalogReplayError::CommitUncertain);
        }
        Err(ScanError::Canceled) => {
            return audit(
                source,
                submission,
                profile,
                admission,
                CatalogReplayError::Canceled,
            );
        }
        Err(_) => {
            return audit(
                source,
                submission,
                profile,
                admission,
                CatalogReplayError::InvalidTarget,
            );
        }
    };
    let Some(checkpoint) = writer.checkpoint.into_inner() else {
        return retained(submission, CatalogReplayError::CommitUncertain);
    };
    let readback = match source.database.read_durable_replay_receipt(
        source_id,
        fence.root_identity(),
        generation,
        generation,
    ) {
        Ok(Some(r)) if r.prior() == &checkpoint && r.source_revision() == delta.revision => r,
        _ => return retained(submission, CatalogReplayError::CommitUncertain),
    };
    let state = source.replay.as_mut().unwrap();
    let pending = state.pending.as_mut().unwrap();
    if state
        .owner
        .mark_applied(pending.dispatched.ticket())
        .is_err()
    {
        return retained(submission, CatalogReplayError::CommitUncertain);
    }
    pending.applied = true;
    if state
        .owner
        .mark_replay_checkpointed(pending.dispatched.ticket(), &checkpoint)
        .is_err()
    {
        return retained(submission, CatalogReplayError::CommitUncertain);
    }
    state.pending = None; // Only the returned durable acknowledgement retires replay raw work.
    patch_snapshot(source, submission.expected_revision, &delta);
    source.replay = None;
    CatalogReplayOutcome::Committed {
        receipt: readback,
        delta,
    }
}
