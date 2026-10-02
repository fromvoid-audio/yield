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
    pub(super) fn request_feature_analysis(
        &mut self,
        source: SourceId,
        lifecycle: u64,
        revision: u64,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.feature_analysis.next_request.checked_add(1) else {
            self.set_playback_status("Feature request identity exhausted");
            return;
        };
        self.feature_analysis.next_request = id;
        self.feature_analysis.pending = None;
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
                self.set_playback_status("Feature request submitted");
                self.start_source_registry_poll(cx);
            }
            Err(error) => self.set_playback_status(format!("Feature request unavailable: {error}")),
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
mod tests {
    use super::*;
    #[test]
    fn feature_projection_rejects_late_source_revision_path_request_and_cancelled_results() {
        let source = SourceId::from_string("source");
        let request = Request {
            source: source.clone(),
            lifecycle: 2,
            revision: 3,
            id: 4,
            path: PathBuf::from("tone.wav"),
            cancellation: FeatureAnalysisCancellation::new(),
            state: None,
        };
        assert!(request.matches(&source, 2, 3, 4, &request.path));
        assert!(!request.matches(&SourceId::from_string("other"), 2, 3, 4, &request.path));
        assert!(!request.matches(&source, 5, 3, 4, &request.path));
        assert!(!request.matches(&source, 2, 5, 4, &request.path));
        assert!(!request.matches(&source, 2, 3, 5, &request.path));
        assert!(!request.matches(&source, 2, 3, 4, &PathBuf::from("other.wav")));
        request.cancellation.cancel();
        assert!(!request.matches(&source, 2, 3, 4, &request.path));
    }
    #[test]
    fn feature_projection_rejects_regression_and_resets_only_for_a_new_fenced_request() {
        let source = SourceId::from_string("source");
        let mut request = Request {
            source: source.clone(),
            lifecycle: 2,
            revision: 3,
            id: 4,
            path: PathBuf::from("tone.wav"),
            cancellation: FeatureAnalysisCancellation::new(),
            state: None,
        };
        let path = request.path.clone();
        // Queued/running are real incoming phases; no terminal key fixture is needed here.
        assert!(request.apply_state(&source, 2, 3, 4, &path, &FeatureAnalysisState::Queued));
        assert!(request.apply_state(&source, 2, 3, 4, &path, &FeatureAnalysisState::Running));
        assert!(!request.apply_state(&source, 2, 3, 4, &path, &FeatureAnalysisState::Queued));
        let terminal = FeatureAnalysisState::Blocked {
            reason: yield_session::FeatureAnalysisBlocker::NotClaimable,
        };
        assert!(!request.apply_state(&source, 2, 4, 4, &path, &terminal));
        assert_eq!(request.state, Some(FeatureAnalysisState::Running));
        assert!(request.apply_state(&source, 2, 3, 4, &path, &terminal));
        assert!(!request.apply_state(&source, 2, 3, 4, &path, &FeatureAnalysisState::Queued));
        // Source revision change creates a fresh request, without accepting old terminal events.
        let mut next = Request {
            source: source.clone(),
            lifecycle: 2,
            revision: 4,
            id: 5,
            path: path.clone(),
            cancellation: FeatureAnalysisCancellation::new(),
            state: None,
        };
        assert!(!next.apply_state(&source, 2, 3, 4, &path, &terminal));
        assert!(next.apply_state(&source, 2, 4, 5, &path, &FeatureAnalysisState::Queued));
        next.cancellation.cancel();
        assert!(!next.apply_state(&source, 2, 4, 5, &path, &FeatureAnalysisState::Running));
    }
    #[test]
    fn replacing_or_dropping_projection_cancels_its_compute_request() {
        let cancellation = FeatureAnalysisCancellation::new();
        let request = Request {
            source: SourceId::from_string("source"),
            lifecycle: 1,
            revision: 1,
            id: 1,
            path: "tone.wav".into(),
            cancellation: cancellation.clone(),
            state: None,
        };
        drop(request);
        assert!(cancellation.is_cancelled());
    }
}
