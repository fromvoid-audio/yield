#[cfg(test)]
pub(super) mod tests {
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
    pub(in crate::browser) fn install(view: &mut Browser, state: FeatureAnalysisState) {
        use super::super::{
            SourceSession, catalog::CatalogActivation, catalog_metadata::CatalogMetadataContext,
        };
        use std::{collections::HashMap, sync::Arc};
        use yield_library::sample_sources::reconciliation::RootIdentity;
        let root = PathBuf::from(format!("/sources/{}", "音楽".repeat(60)));
        let source = SourceId::from_string("source");
        let mut session = SourceSession::new(root.clone());
        session.source_id = Some(source.clone());
        view.source_sessions = vec![session];
        view.active_source = Some(0);
        view.root = Some(root.clone());
        view.catalog_activation = Some(CatalogActivation {
            source_id: source.clone(),
            root: root.clone(),
            lifecycle_generation: 2,
            registered: true,
            registration_admitted: true,
            root_identity: None,
            audit_finished: true,
            audit_error: None,
            audit_retryable: false,
            force_reload: false,
            fallback_to_scan: false,
        });
        view.catalog_metadata
            .install_projection(CatalogMetadataContext {
                source_id: source.clone(),
                lifecycle_generation: 2,
                root,
                root_identity: RootIdentity::from_bytes(b"root".to_vec()),
                directory_generation: 1,
                revision: 3,
                entry_indices: Arc::new(HashMap::new()),
            });
        view.feature_analysis.pending = Some(Request {
            source,
            lifecycle: 2,
            revision: 3,
            id: 4,
            path: PathBuf::from(format!("{}.wav", "長い".repeat(100))),
            cancellation: FeatureAnalysisCancellation::new(),
            state: Some(state),
        });
        view.feature_analysis.next_request = 4;
    }

    #[gpui::test]
    fn feature_job_rows_fence_source_lifecycle_revision_and_rejected_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        use yield_session::FeatureAnalysisBlocker as Blocker;
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(
                view,
                FeatureAnalysisState::Blocked {
                    reason: Blocker::Computation,
                },
            );
            let before = view.feature_job_row().unwrap();
            assert!(before.can_retry);
            view.retry_current_job(cx); // No catalog owner: actual submit rejects.
            assert_eq!(view.feature_job_row(), Some(before));
            assert!(
                view.job_action_error
                    .as_ref()
                    .unwrap()
                    .contains("unavailable")
            );
            let root = view.root.take();
            assert!(view.feature_job_row().is_none());
            view.root = root;
            view.catalog_activation
                .as_mut()
                .unwrap()
                .lifecycle_generation += 1;
            assert!(view.feature_job_row().is_none());
            view.catalog_activation
                .as_mut()
                .unwrap()
                .lifecycle_generation -= 1;
            let mut context = view.catalog_metadata.context().unwrap().clone();
            context.revision += 1;
            view.catalog_metadata.install_projection(context);
            let outdated = view.feature_job_row().unwrap();
            assert!(!outdated.completed);
            assert!(outdated.can_retry);
            view.retry_current_job(cx);
            assert_eq!(view.feature_job_row(), Some(outdated));
            install(
                view,
                FeatureAnalysisState::Blocked {
                    reason: Blocker::UnsupportedPlatform,
                },
            );
            assert!(!view.feature_job_row().unwrap().can_retry);
        });
    }

    #[gpui::test]
    fn feature_events_coalesce_duplicate_repaints_and_project_semantic_counts(
        cx: &mut gpui::TestAppContext,
    ) {
        use yield_session::FeatureAnalysisPhase as Phase;
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(
                view,
                FeatureAnalysisState::Preparing {
                    phase: Phase::InspectingManifest,
                },
            );
            assert!(!view.feature_job_row().unwrap().work.contains(" / "));
            let request = view.feature_analysis.pending.as_ref().unwrap();
            let event = CatalogEvent::FeatureAnalysisState {
                source_id: request.source.clone(),
                lifecycle_generation: 2,
                expected_revision: 3,
                request_id: 4,
                relative_path: request.path.clone(),
                state: FeatureAnalysisState::Preparing {
                    phase: Phase::ComparingReadiness,
                },
            };
            assert!(apply_catalog_event(view, &event, cx));
            let before = view.feature_job_row().unwrap();
            assert!(before.work.contains("0 / 1 readiness targets"));
            for _ in 0..10_000 {
                assert!(apply_catalog_event(view, &event, cx));
            }
            assert_eq!(view.feature_job_row(), Some(before));
            let mut event = event;
            if let CatalogEvent::FeatureAnalysisState { state, .. } = &mut event {
                *state = FeatureAnalysisState::Queued;
            }
            apply_catalog_event(view, &event, cx);
            assert!(
                view.feature_job_row()
                    .unwrap()
                    .work
                    .contains("1 / 1 readiness targets")
            );
            if let CatalogEvent::FeatureAnalysisState { state, .. } = &mut event {
                *state = FeatureAnalysisState::Running;
            }
            apply_catalog_event(view, &event, cx);
            assert!(!view.feature_job_row().unwrap().work.contains(" / "));
            assert_eq!(super::super::job_details::snapshot(view, cx).rows.len(), 1);
        });
    }
}
