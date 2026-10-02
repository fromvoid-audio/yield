//! One explicit sample-menu feature request and truthful, fenced progress projection.
use super::{Browser, catalog};
use gpui::Context;
use std::path::PathBuf;
use yield_library::sample_sources::SourceId;
use yield_session::{
    CatalogCommand, CatalogEvent, FeatureAnalysisCancellation, FeatureAnalysisState,
};

#[derive(Default)]
pub(super) struct FeatureAnalysisProjection {
    next_request: u64,
    pending: Option<Request>,
}
struct Request {
    source: SourceId,
    lifecycle: u64,
    revision: u64,
    id: u64,
    path: PathBuf,
    cancellation: FeatureAnalysisCancellation,
    state: Option<FeatureAnalysisState>,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
impl Request {
    fn matches(
        &self,
        source: &SourceId,
        lifecycle: u64,
        revision: u64,
        id: u64,
        path: &PathBuf,
    ) -> bool {
        !self.cancellation.is_cancelled()
            && self.source == *source
            && self.lifecycle == lifecycle
            && self.revision == revision
            && self.id == id
            && self.path == *path
    }
    fn apply_state(
        &mut self,
        source: &SourceId,
        lifecycle: u64,
        revision: u64,
        id: u64,
        path: &PathBuf,
        state: &FeatureAnalysisState,
    ) -> bool {
        if !self.matches(source, lifecycle, revision, id, path)
            || !state.follows(self.state.as_ref())
        {
            return false;
        }
        self.state = Some(state.clone());
        true
    }
}
impl Browser {
    // Observe a revision change only within the same current audited owner.
    // Navigation, retirement and incomplete audits still hide the old request.
    fn feature_request_context(&self) -> Option<(&Request, u64)> {
        let request = self.feature_analysis.pending.as_ref()?;
        let activation = self.catalog_activation.as_ref()?;
        let context = self.catalog_metadata.context()?;
        let session = self.source_sessions.get(self.active_source?)?;
        (self.root.as_ref() == Some(&activation.root)
            && context.root == activation.root
            && session.source_id.as_ref() == Some(&request.source)
            && session.root == activation.root
            && activation.source_id == request.source
            && activation.lifecycle_generation == request.lifecycle
            && activation.registered
            && activation.registration_admitted
            && activation.audit_finished
            && activation.audit_error.is_none()
            && context.source_id == request.source
            && context.lifecycle_generation == request.lifecycle
            && context.revision >= request.revision
            // A later audited revision can explain why the old attempt was
            // cancelled. Only its revalidation row is visible, never its state.
            && (context.revision > request.revision || !request.cancellation.is_cancelled()))
        .then_some((request, context.revision))
    }

    fn current_feature_request(&self) -> Option<&Request> {
        let (request, revision) = self.feature_request_context()?;
        (request.revision == revision).then_some(request)
    }

    pub(super) fn feature_job_row(&self) -> Option<super::job_details::JobRow> {
        use super::job_details::{FeatureJobState, JobKind, JobRow, bounded_text};
        use yield_session::FeatureAnalysisBlocker as Blocker;
        let (request, revision) = self.feature_request_context()?;
        let outdated = revision != request.revision;
        let feature_state = if outdated {
            FeatureJobState::NeedsRevalidation
        } else {
            match request.state {
                None | Some(FeatureAnalysisState::Preparing { .. }) => FeatureJobState::Preparing,
                Some(FeatureAnalysisState::Queued) => FeatureJobState::Queued,
                Some(FeatureAnalysisState::Running) => FeatureJobState::Running,
                Some(FeatureAnalysisState::Ready { .. }) => FeatureJobState::Ready,
                Some(FeatureAnalysisState::Blocked { .. }) => FeatureJobState::Blocked,
            }
        };
        let completed = feature_state == FeatureJobState::Ready;
        let blocked = if outdated {
            Some("This source was updated; retry to check this file again".into())
        } else {
            match &request.state {
                Some(FeatureAnalysisState::Blocked { reason }) => Some(reason.message().to_owned()),
                _ => None,
            }
        };
        let can_retry = outdated
            || matches!(
                request.state,
                Some(FeatureAnalysisState::Blocked {
                    reason: Blocker::AdmissionBudget
                        | Blocker::InputChanged
                        | Blocker::Computation
                        | Blocker::NotClaimable
                        | Blocker::Cancelled
                        | Blocker::Publication,
                })
            );
        let activity = if outdated {
            "Needs revalidation".into()
        } else {
            request
                .state
                .as_ref()
                .map(FeatureAnalysisState::message)
                .unwrap_or_else(|| "Feature request submitted".into())
        };
        let file = bounded_text(&request.path.to_string_lossy(), 48);
        Some(JobRow {
            kind: JobKind::FeatureAnalysis,
            feature_state: Some(feature_state),
            source: self.root.clone(),
            generation: request.id,
            work: format!("{activity} · {file}"),
            completed,
            blocked,
            action: None,
            can_retry,
            can_cancel: false,
        })
    }

    fn feature_retry_target(&self) -> Option<(SourceId, u64, u64, PathBuf)> {
        if !self.feature_job_row()?.can_retry {
            return None;
        }
        let (request, revision) = self.feature_request_context()?;
        Some((
            request.source.clone(),
            request.lifecycle,
            revision,
            request.path.clone(),
        ))
    }

    pub(super) fn retry_current_job(&mut self, cx: &mut Context<Self>) {
        if let Some((source, lifecycle, revision, path)) = self.feature_retry_target() {
            self.request_feature_analysis(source, lifecycle, revision, path, cx);
        } else {
            self.retry_job_audit(cx);
        }
    }

    pub(super) fn request_feature_analysis(
        &mut self,
        source: SourceId,
        lifecycle: u64,
        revision: u64,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.feature_analysis.next_request.checked_add(1) else {
            let message = "Feature request identity exhausted";
            self.job_action_error = Some(message.into());
            self.set_playback_status(message);
            cx.notify();
            return;
        };
        self.feature_analysis.next_request = id;
        let cancellation = FeatureAnalysisCancellation::new();
        let command = CatalogCommand::RequestFeatureAnalysis {
            source_id: source.clone(),
            lifecycle_generation: lifecycle,
            expected_revision: revision,
            request_id: id,
            relative_path: path.clone(),
            cancellation: cancellation.clone(),
        };
        match catalog::submit_catalog_command(command, cx) {
            Ok(()) => {
                self.feature_analysis.pending = Some(Request {
                    source,
                    lifecycle,
                    revision,
                    id,
                    path,
                    cancellation,
                    state: None,
                });
                self.job_action_error = None;
                self.set_playback_status("Feature request submitted");
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                let message = format!("Feature request unavailable: {error}");
                self.job_action_error = Some(message.clone());
                self.set_playback_status(message);
            }
        }
        cx.notify();
    }
}
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::FeatureAnalysisState {
        source_id,
        lifecycle_generation,
        expected_revision,
        request_id,
        relative_path,
        state,
    } = event
    else {
        return false;
    };
    if browser.current_feature_request().is_none() {
        return true;
    }
    let Some(request) = browser.feature_analysis.pending.as_mut() else {
        return true;
    };
    if !browser.catalog_activation.as_ref().is_some_and(|a| {
        a.source_id == *source_id
            && a.lifecycle_generation == *lifecycle_generation
            && a.audit_finished
            && a.audit_error.is_none()
    }) || !browser.catalog_metadata.context().is_some_and(|c| {
        c.source_id == *source_id
            && c.lifecycle_generation == *lifecycle_generation
            && c.revision == *expected_revision
    }) {
        return true;
    }
    if !request.apply_state(
        source_id,
        *lifecycle_generation,
        *expected_revision,
        *request_id,
        relative_path,
        state,
    ) {
        return true;
    }
    let text = state.message();
    browser.set_playback_status(text);
    cx.notify();
    true
}

#[cfg(test)]
include!("analysis_tests.rs");

#[cfg(test)]
include!("analysis_lifecycle_tests.rs");

#[cfg(test)]
include!("analysis_presentation_tests.rs");
