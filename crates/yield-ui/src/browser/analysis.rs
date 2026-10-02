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
    if !request.matches(
        source_id,
        *lifecycle_generation,
        *expected_revision,
        *request_id,
        relative_path,
    ) || !browser.catalog_activation.as_ref().is_some_and(|a| {
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
    if matches!(
        &request.state,
        Some(FeatureAnalysisState::Ready { .. } | FeatureAnalysisState::Blocked { .. })
    ) {
        return true;
    }
    request.state = Some(state.clone());
    let text = match state {
        FeatureAnalysisState::Queued => "Features queued".to_owned(),
        FeatureAnalysisState::Running => "Analyzing features".to_owned(),
        FeatureAnalysisState::Ready { .. } => "Features ready".to_owned(),
        FeatureAnalysisState::Blocked { reason } => {
            format!("Features blocked: {}", reason.message())
        }
    };
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
