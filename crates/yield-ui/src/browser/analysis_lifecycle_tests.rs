// Headless projection/reducer regressions; these issue no physical capability.
#[cfg(test)]
mod lifecycle_tests {
    use super::tests::install;
    use super::*;
    use gpui::TestAppContext;
    use yield_session::{FeatureAnalysisBlocker as Blocker, FeatureAnalysisPhase as Phase};

    fn event(request: &Request, state: FeatureAnalysisState) -> CatalogEvent {
        CatalogEvent::FeatureAnalysisState {
            source_id: request.source.clone(),
            lifecycle_generation: request.lifecycle,
            expected_revision: request.revision,
            request_id: request.id,
            relative_path: request.path.clone(),
            state,
        }
    }

    #[gpui::test]
    fn current_progress_rejects_each_obsolete_event_fence(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(view, FeatureAnalysisState::Queued);
            let before = view.feature_job_row().unwrap();
            for mismatch in 0..5 {
                let mut late = event(
                    view.feature_analysis.pending.as_ref().unwrap(),
                    FeatureAnalysisState::Running,
                );
                if let CatalogEvent::FeatureAnalysisState {
                    source_id,
                    lifecycle_generation,
                    expected_revision,
                    request_id,
                    relative_path,
                    ..
                } = &mut late
                {
                    match mismatch {
                        0 => *source_id = SourceId::from_string("retired-source"),
                        1 => *lifecycle_generation += 1,
                        2 => *expected_revision += 1,
                        3 => *request_id += 1,
                        _ => *relative_path = "other.wav".into(),
                    }
                }
                for _ in 0..100 {
                    assert!(apply_catalog_event(view, &late, cx));
                }
                assert_eq!(view.feature_job_row(), Some(before.clone()));
                assert_eq!(
                    view.feature_analysis.pending.as_ref().unwrap().state,
                    Some(FeatureAnalysisState::Queued)
                );
            }
        });
    }

    #[gpui::test]
    fn withdrawn_source_or_audit_context_cannot_accept_late_progress(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            for mismatch in 0..8 {
                install(view, FeatureAnalysisState::Queued);
                let late = event(
                    view.feature_analysis.pending.as_ref().unwrap(),
                    FeatureAnalysisState::Running,
                );
                match mismatch {
                    0 => view.catalog_activation.as_mut().unwrap().registered = false,
                    1 => {
                        view.catalog_activation
                            .as_mut()
                            .unwrap()
                            .registration_admitted = false
                    }
                    2 => view.catalog_activation.as_mut().unwrap().audit_finished = false,
                    3 => {
                        view.catalog_activation.as_mut().unwrap().audit_error =
                            Some("Audit incomplete".into())
                    }
                    4 => view.active_source = None,
                    5 => {
                        view.source_sessions[0].source_id =
                            Some(SourceId::from_string("replacement"))
                    }
                    6 => view.source_sessions[0].root = "/replacement".into(),
                    _ => {
                        let mut context = view.catalog_metadata.context().unwrap().clone();
                        context.root = "/replacement".into();
                        view.catalog_metadata.install_projection(context);
                    }
                }
                assert!(view.feature_job_row().is_none());
                apply_catalog_event(view, &late, cx);
                assert_eq!(
                    view.feature_analysis.pending.as_ref().unwrap().state,
                    Some(FeatureAnalysisState::Queued)
                );
                view.retry_current_job(cx);
                assert!(view.feature_job_row().is_none());
                assert_eq!(view.feature_analysis.next_request, 4);
            }
        });
    }

    #[gpui::test]
    fn replacement_cancels_old_request_and_resets_only_the_new_run(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(
                view,
                FeatureAnalysisState::Blocked {
                    reason: Blocker::NotClaimable,
                },
            );
            let old = view.feature_analysis.pending.as_ref().unwrap();
            let old_token = old.cancellation.clone();
            let late = event(old, FeatureAnalysisState::Running);
            // Model the already-admitted new run's projection. Actual owner admission
            // and artifact publication are covered by catalog feature request tests.
            install(view, FeatureAnalysisState::Queued);
            assert!(old_token.is_cancelled());
            let next = view.feature_analysis.pending.as_mut().unwrap();
            next.id = 5;
            next.lifecycle = 3;
            next.revision = 4;
            next.state = None;
            view.catalog_activation
                .as_mut()
                .unwrap()
                .lifecycle_generation = 3;
            let mut context = view.catalog_metadata.context().unwrap().clone();
            context.lifecycle_generation = 3;
            context.revision = 4;
            view.catalog_metadata.install_projection(context);
            apply_catalog_event(view, &late, cx);
            assert!(
                view.feature_analysis
                    .pending
                    .as_ref()
                    .unwrap()
                    .state
                    .is_none()
            );
            let first = event(
                view.feature_analysis.pending.as_ref().unwrap(),
                FeatureAnalysisState::Preparing {
                    phase: Phase::InspectingManifest,
                },
            );
            apply_catalog_event(view, &first, cx);
            assert!(view.feature_job_row().unwrap().work.contains("Inspecting"));
            let queued = event(
                view.feature_analysis.pending.as_ref().unwrap(),
                FeatureAnalysisState::Queued,
            );
            apply_catalog_event(view, &queued, cx);
            assert!(
                view.feature_job_row()
                    .unwrap()
                    .work
                    .contains("1 / 1 readiness targets")
            );
            // A real cancellation token fences even matching late events. No worker
            // scheduling/ownership or positive completion semantics change here.
            view.feature_analysis
                .pending
                .as_ref()
                .unwrap()
                .cancellation
                .cancel();
            let running = event(
                view.feature_analysis.pending.as_ref().unwrap(),
                FeatureAnalysisState::Running,
            );
            apply_catalog_event(view, &running, cx);
            assert!(view.feature_job_row().is_none());
            assert_eq!(
                view.feature_analysis.pending.as_ref().unwrap().state,
                Some(FeatureAnalysisState::Queued)
            );
            view.feature_analysis.pending = None;
            assert_eq!(
                super::super::job_details::snapshot(view, cx).status_label(),
                "Jobs idle"
            );
        });
    }

    #[gpui::test]
    fn terminal_blocked_and_exhausted_retry_preserve_the_current_request(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(
                view,
                FeatureAnalysisState::Blocked {
                    reason: Blocker::Cancelled,
                },
            );
            let before = view.feature_job_row().unwrap();
            assert!(before.can_retry);
            let late = event(
                view.feature_analysis.pending.as_ref().unwrap(),
                FeatureAnalysisState::Running,
            );
            apply_catalog_event(view, &late, cx);
            assert_eq!(view.feature_job_row(), Some(before.clone()));
            view.retry_current_job(cx); // Real unavailable owner rejection; no modeled success.
            assert_eq!(view.feature_analysis.next_request, 5);
            assert_eq!(view.feature_analysis.pending.as_ref().unwrap().id, 4);
            assert!(
                !view
                    .feature_analysis
                    .pending
                    .as_ref()
                    .unwrap()
                    .cancellation
                    .is_cancelled()
            );
            assert_eq!(view.feature_job_row(), Some(before.clone()));
            view.feature_analysis.next_request = u64::MAX;
            view.job_action_error = None;
            view.retry_current_job(cx);
            assert_eq!(view.feature_analysis.next_request, u64::MAX);
            assert_eq!(view.feature_job_row(), Some(before));
            assert_eq!(
                view.job_action_error.as_deref(),
                Some("Feature request identity exhausted")
            );
        });
    }

    #[test]
    fn duplicate_phase_and_late_terminal_batches_reduce_to_six_updates() {
        let source = SourceId::from_string("source");
        let path = PathBuf::from("tone.wav");
        let mut request = Request {
            source: source.clone(),
            lifecycle: 2,
            revision: 3,
            id: 4,
            path: path.clone(),
            cancellation: FeatureAnalysisCancellation::new(),
            state: None,
        };
        let states = [
            FeatureAnalysisState::Preparing {
                phase: Phase::InspectingManifest,
            },
            FeatureAnalysisState::Preparing {
                phase: Phase::ComparingReadiness,
            },
            FeatureAnalysisState::Preparing {
                phase: Phase::QueueingUnfinishedWork,
            },
            FeatureAnalysisState::Queued,
            FeatureAnalysisState::Running,
            FeatureAnalysisState::Blocked {
                reason: Blocker::Computation,
            },
        ];
        let mut accepted = 0;
        for state in &states {
            for _ in 0..1_000 {
                accepted += usize::from(request.apply_state(&source, 2, 3, 4, &path, state));
            }
        }
        assert_eq!(accepted, 6);
        for state in states.iter().rev() {
            assert!(!request.apply_state(&source, 2, 3, 4, &path, state));
        }
        assert_eq!(request.state, states.last().cloned());
    }
}
