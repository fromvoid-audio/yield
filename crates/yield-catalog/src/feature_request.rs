//! Production single-job request through the retained catalog owner, with bounded output.
use super::*;
use yield_library::sample_sources::readiness::{
    FeatureArtifactKey, FeatureArtifactRebuildAdmission, ReadinessClassification, ReadinessDeficit,
    SourceFeatureInputError,
};

/// Cancellation of one analysis request, independent of a readiness artifact identity.
#[derive(Clone, Debug, Default)]
pub struct FeatureAnalysisCancellation(Arc<AtomicBool>);
impl FeatureAnalysisCancellation {
    /// Create one uncancelled request fence.
    pub fn new() -> Self {
        Self::default()
    }
    /// Stop this request before durable publication; already committed evidence is retained.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Whether this request has been cancelled or retired.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Bounded reason why one feature request cannot report ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureAnalysisBlocker {
    /// Atomic preparation exceeded its bounded work admission; previous evidence is retained.
    AdmissionBudget,
    /// Required source owner, lifecycle, root or revision no longer matches.
    StaleSource,
    /// Source target admission lacks current full hash/identity/audit or exceeds its bounds.
    TargetUnavailable,
    /// Held input cannot be implemented by the current platform adapter.
    UnsupportedPlatform,
    /// The source input changed or was unavailable.
    InputChanged,
    /// The existing decoder/feature engine failed for these bytes.
    Computation,
    /// A current claim is unavailable, including a retained retry/lease or missing-payload rebuild.
    NotClaimable,
    /// Exact payload/work provenance cannot be safely repaired; evidence is retained.
    ArtifactOwnership,
    /// Explicit cancellation or source retirement stopped this request.
    Cancelled,
    /// Durable source publication or validated artifact lookup failed.
    Publication,
}
impl FeatureAnalysisBlocker {
    /// User-facing explanation of this request's blocked state.
    pub fn message(self) -> &'static str {
        match self {
            Self::AdmissionBudget => "Feature preparation reached its work limit; try again",
            Self::StaleSource => "The source changed; refresh and try again",
            Self::TargetUnavailable => "The audited source is not available for this request",
            Self::UnsupportedPlatform => "Analysis input is unavailable on this platform",
            Self::InputChanged => "The file changed or could not be read",
            Self::Computation => "Feature analysis failed for this file",
            Self::NotClaimable => "Feature work is waiting for retry or rebuild",
            Self::ArtifactOwnership => "Feature payload ownership needs review; evidence retained",
            Self::Cancelled => "The feature request was cancelled",
            Self::Publication => "The feature result could not be verified",
        }
    }
}
/// Truthful state of one file-scoped AnalysisFeatures request, not overall source readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureAnalysisState {
    /// Indeterminate owner activity; no feature work or readiness is claimed complete.
    Preparing {
        /// Current semantic preparation phase.
        phase: FeatureAnalysisPhase,
    },
    /// The exact owner-derived target was durably queued.
    Queued,
    /// Its exact claim and held input were prepared; computation has not completed.
    Running,
    /// Actual stored bytes were validated through the current retained owner.
    Ready {
        /// Content/version-bound key reissued after validated lookup; not a writable capability.
        key: FeatureArtifactKey,
    },
    /// This request cannot report ready; durable retry evidence is retained when a claim existed.
    Blocked {
        /// Typed, bounded failure or unavailable resource.
        reason: FeatureAnalysisBlocker,
    },
}

pub(super) fn request(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    relative_path: PathBuf,
    cancellation: FeatureAnalysisCancellation,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
    _test_gate: Option<&Arc<CatalogTestGate>>,
) {
    let started = Instant::now();
    let token = Arc::clone(&cancellation.0);
    let begun = {
        let mut state = admission.lock().unwrap_or_else(|p| p.into_inner());
        state.feature_analysis_metrics.enter();
        (|| {
            if state.shutdown || state.closed {
                return false;
            }
            let Some(source) = state.sources.get_mut(&source_id) else {
                return false;
            };
            if !source.active || source.lifecycle_generation != lifecycle_generation {
                return false;
            }
            if let Some(previous) = source.attempt_token.replace(Arc::clone(&token)) {
                previous.store(true, Ordering::Release);
            }
            true
        })()
    };
    if !begun {
        admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .feature_analysis_metrics
            .drop_before_work(started.elapsed());
        return;
    }
    let Some(source) = sources.get_mut(&source_id) else {
        admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .feature_analysis_metrics
            .drop_before_work(started.elapsed());
        return;
    };
    source.attempt_token = Some(Arc::clone(&token));
    let emit = |reservation: &mut EventReservation<'_>, source: &RegisteredSource, state| {
        let terminal = feature_analysis_metrics::TerminalObservation::from_state(&state);
        #[cfg(test)]
        if terminal.is_some() {
            if let Some(gate) = _test_gate {
                gate.before_feature_terminal();
            }
        }
        // Cancelled is still a truthful terminal result for the current source. The physical
        // publication fence and caller request/revision checks reject late ready projections.
        let enqueued = publish_source_if_current(
            admission,
            reservation,
            &source_id,
            lifecycle_generation,
            None,
            source,
            CatalogEvent::FeatureAnalysisState {
                source_id: source_id.clone(),
                lifecycle_generation,
                expected_revision,
                request_id,
                relative_path: relative_path.clone(),
                state,
            },
        );
        if let Some(terminal) = terminal {
            admission
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .feature_analysis_metrics
                .terminal(terminal, enqueued, started.elapsed());
        }
    };
    let blocked = |reason| FeatureAnalysisState::Blocked { reason };
    let now = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(i64::MAX as u64) as i64
    };
    let revision = match source.database.writer.current_revision_for_command() {
        Ok(revision)
            if revision.revision() == expected_revision
                && source.lifecycle_generation == lifecycle_generation =>
        {
            revision
        }
        _ => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::StaleSource),
            );
            return;
        }
    };
    if cancellation.is_cancelled() {
        emit(
            reservation,
            source,
            blocked(FeatureAnalysisBlocker::Cancelled),
        );
        return;
    }
    emit(
        reservation,
        source,
        FeatureAnalysisState::Preparing {
            phase: FeatureAnalysisPhase::InspectingManifest,
        },
    );
    let target = match source.database.writer.prepare_feature_readiness_target(
        &revision,
        &relative_path,
        now(),
        &token,
    ) {
        Ok(target) => target,
        Err(SourceWriterError::Readiness(
            yield_library::sample_sources::readiness::ReadinessError::AdmissionBudget(_),
        )) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::AdmissionBudget),
            );
            return;
        }
        Err(_) => {
            emit(
                reservation,
                source,
                blocked(if cancellation.is_cancelled() {
                    FeatureAnalysisBlocker::Cancelled
                } else {
                    FeatureAnalysisBlocker::TargetUnavailable
                }),
            );
            return;
        }
    };
    emit(
        reservation,
        source,
        FeatureAnalysisState::Preparing {
            phase: FeatureAnalysisPhase::ComparingReadiness,
        },
    );
    match source
        .database
        .writer
        .lookup_feature_artifact_for_target(&revision, &target)
    {
        Ok(Some((key, _))) if !cancellation.is_cancelled() => {
            emit(reservation, source, FeatureAnalysisState::Ready { key });
            return;
        }
        Err(_) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::Publication),
            );
            return;
        }
        _ => {}
    }
    match source
        .database
        .writer
        .admit_feature_artifact_rebuild(&revision, &target, now(), &token)
    {
        Ok(
            FeatureArtifactRebuildAdmission::Ready
            | FeatureArtifactRebuildAdmission::Admitted { .. }
            | FeatureArtifactRebuildAdmission::Unchanged,
        ) => {}
        Ok(FeatureArtifactRebuildAdmission::Deferred) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::NotClaimable),
            );
            return;
        }
        Ok(FeatureArtifactRebuildAdmission::Ambiguous) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::ArtifactOwnership),
            );
            return;
        }
        Ok(FeatureArtifactRebuildAdmission::RejectedStale) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::StaleSource),
            );
            return;
        }
        Err(_) => {
            emit(
                reservation,
                source,
                blocked(if cancellation.is_cancelled() {
                    FeatureAnalysisBlocker::Cancelled
                } else {
                    FeatureAnalysisBlocker::Publication
                }),
            );
            return;
        }
    }
    emit(
        reservation,
        source,
        FeatureAnalysisState::Preparing {
            phase: FeatureAnalysisPhase::QueueingUnfinishedWork,
        },
    );
    let deficit = ReadinessDeficit {
        target: target.clone(),
        reason: ReadinessClassification::Pending,
        enqueued_at: None,
    };
    if source
        .database
        .writer
        .enqueue_readiness_jobs(&revision, &[deficit], now())
        .is_err()
    {
        emit(
            reservation,
            source,
            blocked(FeatureAnalysisBlocker::Publication),
        );
        return;
    }
    let job = match prepare_feature_job(&mut source.database.writer, &target, now(), 120) {
        Ok(Some(job)) => job,
        Ok(None) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::NotClaimable),
            );
            return;
        }
        Err(SourceWriterError::FeatureInput(SourceFeatureInputError::UnsupportedPlatform)) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::UnsupportedPlatform),
            );
            return;
        }
        Err(_) => {
            emit(
                reservation,
                source,
                blocked(FeatureAnalysisBlocker::InputChanged),
            );
            return;
        }
    };
    emit(reservation, source, FeatureAnalysisState::Queued);
    emit(reservation, source, FeatureAnalysisState::Running);
    // This is the catalog's background worker; no source SQL transaction is held during I/O/DSP.
    let result = job
        .compute(&token)
        .finish(&mut source.database.writer, now(), &token);
    let state = match result {
        Ok(FeatureJobOutcome::Published(_)) => match source
            .database
            .writer
            .current_revision()
            .and_then(|revision| {
                source
                    .database
                    .writer
                    .lookup_feature_artifact_for_target(&revision, &target)
            }) {
            Ok(Some((key, _))) if !cancellation.is_cancelled() => {
                FeatureAnalysisState::Ready { key }
            }
            _ => blocked(FeatureAnalysisBlocker::Publication),
        },
        Ok(FeatureJobOutcome::Cancelled) => blocked(FeatureAnalysisBlocker::Cancelled),
        Ok(FeatureJobOutcome::RejectedStale) => blocked(FeatureAnalysisBlocker::StaleSource),
        Ok(FeatureJobOutcome::Failed(FeatureJobFailure::Computation)) => {
            blocked(FeatureAnalysisBlocker::Computation)
        }
        Ok(FeatureJobOutcome::Failed(_)) => blocked(FeatureAnalysisBlocker::InputChanged),
        Err(_) => blocked(if cancellation.is_cancelled() {
            FeatureAnalysisBlocker::Cancelled
        } else {
            FeatureAnalysisBlocker::Publication
        }),
    };
    emit(reservation, source, state);
}
