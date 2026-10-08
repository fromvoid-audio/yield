//! Owner-driven history requests. Native callbacks never borrow the catalog or perform SQL.
use super::*;
use crate::replay::{CatalogReplayOutcome, consume_captured_on_worker};
use yield_library::sample_sources::reconciliation::WatcherGeneration;
use yield_source_watcher::replay::{
    CapturedNativeReplayBatch, NativeReplayCaptureError, NativeReplayCaptureRequest,
};

pub(super) struct NativeReplayWork {
    request: Option<NativeReplayCaptureRequest>,
    batch: Option<Arc<CapturedNativeReplayBatch>>,
    pending: PendingWatcherHint,
    expected_revision: u64,
    retry_after: Instant,
    empty_retry_until: Instant,
}

/// Start at most one original history producer; retain every undecided result on its source.
pub(super) fn run(
    profile: &WritableProfileGuard,
    events: &Arc<EventHub>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<super::super::SourceId, RegisteredSource>,
    mode: WatcherMode,
    now: Instant,
) {
    if !matches!(mode, WatcherMode::Native) || !cfg!(target_os = "macos") {
        return;
    }
    // A retained envelope wins over a later hint. New hints stay coalesced for the next capture.
    if let Some(id) = sources.iter().find_map(|(id, source)| {
        source
            .watcher_schedule
            .native_replay
            .as_ref()
            .map(|_| id.clone())
    }) {
        poll(profile, events, admission, sources, &id, now);
        return;
    }
    let Some(id) = sources.iter().find_map(|(id, source)| {
        let schedule = &source.watcher_schedule;
        let pending = schedule.pending.as_ref()?;
        (!schedule.initial_audit_pending
            && schedule.root_issue.is_none()
            && !schedule.watcher_continuity_lost
            && source.watcher.is_some()
            && source.replay.is_none()
            && schedule.pending_is_due(now)
            && !pending.evidence.requires_full_rescan()
            && !pending.evidence.has_backend_error())
        .then(|| id.clone())
    }) else {
        return;
    };
    let source = sources.get_mut(&id).expect("selected retained source");
    let life = source.lifecycle_generation;
    if !publication_is_current(admission, &id, life, None)
        || profile.validate_current().is_err()
        || source.database.validate_profile_guard(profile).is_err()
        || source.publication_fence.validate().is_err()
    {
        return;
    }
    let Ok(fence) = source.database.writer.current_scan_fence() else {
        return;
    };
    let generation = WatcherGeneration::new(life);
    // This is actual persisted authority read under the registered owner, never a hint cursor.
    let Ok(Some(prior)) = source.database.read_durable_replay_prior(
        &id,
        fence.root_identity(),
        generation,
        generation,
    ) else {
        return;
    };
    let spec = SourceWatchSpec::new(
        id.clone(),
        life,
        source.root.clone(),
        Some(fence.root_identity().clone()),
    );
    match NativeReplayCaptureRequest::start(
        spec,
        prior.acknowledged_sequence(),
        Duration::from_secs(2),
    ) {
        Ok(request) => {
            let pending = source
                .watcher_schedule
                .pending
                .take()
                .expect("selected hint");
            source.watcher_schedule.native_replay = Some(NativeReplayWork {
                request: Some(request),
                batch: None,
                pending,
                expected_revision: fence.revision(),
                retry_after: now,
                empty_retry_until: now + Duration::from_secs(5),
            });
        }
        Err(NativeReplayCaptureError::Busy) => {
            // A stopped source can still have an actually running native worker. Do not spin,
            // fabricate quiescence, or replace its custody with an audit on this turn.
            source.watcher_schedule.audit_retry_after = Some(now + WATCHER_RETRY_MIN);
        }
        Err(_) => source.watcher_schedule.queue_replay_audit(),
    }
}

fn poll(
    profile: &WritableProfileGuard,
    events: &Arc<EventHub>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<super::super::SourceId, RegisteredSource>,
    id: &super::super::SourceId,
    now: Instant,
) {
    let source = sources.get_mut(id).expect("selected retained source");
    let life = source.lifecycle_generation;
    if !publication_is_current(admission, id, life, None) {
        // Dropping this request only cancels delivery. The native worker keeps its own slot.
        source.watcher_schedule.native_replay = None;
        return;
    }
    let work = source.watcher_schedule.native_replay.as_mut().unwrap();
    if work.request.is_none() && work.batch.is_none() {
        if now >= work.empty_retry_until {
            let work = source.watcher_schedule.native_replay.take().unwrap();
            source.watcher_schedule.merge_pending(work.pending);
            source.watcher_schedule.queue_replay_audit();
            return;
        }
        if now < work.retry_after {
            return;
        }
        // notify can deliver a hint before FSEvents publishes that change to device history.
        // Certain empty history must not discard the hint. Re-read the actual owner/prior
        // for a bounded retry; only the existing audit can recover after the retry deadline.
        let restart = (|| {
            profile
                .validate_current()
                .map_err(|_| NativeReplayCaptureError::HistoryUnavailable)?;
            source
                .database
                .validate_profile_guard(profile)
                .map_err(|_| NativeReplayCaptureError::HistoryUnavailable)?;
            source
                .publication_fence
                .validate()
                .map_err(|_| NativeReplayCaptureError::RootChanged)?;
            let fence = source
                .database
                .writer
                .current_scan_fence()
                .map_err(|_| NativeReplayCaptureError::HistoryUnavailable)?;
            let generation = WatcherGeneration::new(life);
            let prior = source
                .database
                .read_durable_replay_prior(id, fence.root_identity(), generation, generation)
                .map_err(|_| NativeReplayCaptureError::HistoryUnavailable)?
                .ok_or(NativeReplayCaptureError::HistoryUnavailable)?;
            let spec = SourceWatchSpec::new(
                id.clone(),
                life,
                source.root.clone(),
                Some(fence.root_identity().clone()),
            );
            let request = NativeReplayCaptureRequest::start(
                spec,
                prior.acknowledged_sequence(),
                work.empty_retry_until
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(2)),
            )?;
            Ok::<_, NativeReplayCaptureError>((request, fence.revision()))
        })();
        match restart {
            Ok((request, revision)) => {
                work.request = Some(request);
                work.expected_revision = revision;
            }
            Err(NativeReplayCaptureError::Busy) => {
                work.retry_after = now + WATCHER_RETRY_MIN;
            }
            Err(_) => {
                let work = source.watcher_schedule.native_replay.take().unwrap();
                source.watcher_schedule.merge_pending(work.pending);
                source.watcher_schedule.queue_replay_audit();
            }
        }
        return;
    }
    if let Some(request) = work.request.as_mut() {
        match request.try_result() {
            Ok(None) => return,
            Ok(Some(captured)) => {
                work.request = None;
                let Some(batch) =
                    captured.into_observation_batch(life, WatcherGeneration::new(life))
                else {
                    // A late native history publication is possible after HistoryDone.
                    // Keep the original hint without acknowledging or manufacturing a delta.
                    work.retry_after = now + WATCHER_RETRY_MIN;
                    return;
                };
                work.batch = Some(Arc::new(batch));
            }
            Err(_) => {
                let work = source.watcher_schedule.native_replay.take().unwrap();
                source.watcher_schedule.merge_pending(work.pending);
                source.watcher_schedule.queue_replay_audit();
                return;
            }
        }
    }
    if now < work.retry_after {
        return;
    }
    // Reserve the existing bounded delivery slot before any replay mutation. Holding a
    // capture while UI delivery is busy consumes no extra event or worker-thread slot.
    {
        let mut state = admission.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.closed || state.event_budget != 1 {
            return;
        }
        state.event_budget += 1;
    }
    let mut reservation = EventReservation {
        hub: events,
        admission,
        remaining: 1,
        curation_payload_bytes: 0,
        curation_update_count: 0,
        tag_read_payload_bytes: 0,
    };
    let mut work = source.watcher_schedule.native_replay.take().unwrap();
    let outcome = consume_captured_on_worker(
        work.batch.as_ref().unwrap().clone(),
        life,
        work.expected_revision,
        profile,
        admission,
        sources,
    );
    let source = sources
        .get_mut(id)
        .expect("consumer preserves source registration");
    match outcome {
        Ok(CatalogReplayOutcome::Committed { receipt, delta }) => {
            if publication_is_current(admission, id, life, None) {
                reservation.publish_with_fence(
                    super::super::CatalogEvent::ReplayCommitted {
                        source_id: id.clone(),
                        lifecycle_generation: life,
                        root_identity: receipt.prior().root_identity().clone(),
                        expected_revision: work.expected_revision,
                        receipt,
                        delta,
                    },
                    Some(source.publication_fence.clone()),
                );
            }
        }
        Ok(CatalogReplayOutcome::PreviouslyCommitted { receipt }) => {
            // The persisted receipt covers this exact interval. There is no new projection
            // patch to invent after a duplicate or lost response.
            if source
                .snapshot
                .as_ref()
                .is_none_or(|s| s.committed_revision != receipt.source_revision())
            {
                source.watcher_schedule.queue_replay_audit();
            }
        }
        Ok(CatalogReplayOutcome::AuditRequired { .. }) => {
            source.watcher_schedule.merge_pending(work.pending);
            source.watcher_schedule.queue_replay_audit();
        }
        Ok(CatalogReplayOutcome::Retained { .. }) | Err(_) => {
            work.retry_after = now + WATCHER_RETRY_MIN;
            source.watcher_schedule.native_replay = Some(work);
        }
    }
    reservation.finish();
}
