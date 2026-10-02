// Actual GPUI projection tests. The key below comes from the production catalog,
// retained SQLite owner and WAV/DSP route, not a public fixture key constructor.
#[cfg(test)]
mod presentation_tests {
    use super::super::job_details::{FeatureJobState, snapshot};
    use super::tests::install;
    use super::*;
    use gpui::TestAppContext;
    use yield_session::FeatureAnalysisBlocker as Blocker;

    fn published_ready() -> FeatureAnalysisState {
        use std::time::{Duration, Instant};
        use yield_catalog::CatalogService;
        use yield_library::app_dirs::{
            ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard,
        };
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("analysis-presentation");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        // One second of valid mono PCM, through the existing decoder/feature engine.
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(36u32 + 32000).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&32000u32.to_le_bytes());
        for sample in 0..16000i16 {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(root.path().join("tone.wav"), wav).unwrap();
        let source = SourceId::from_string("analysis-presentation");
        let service = CatalogService::start(&guard).unwrap();
        let receive = |predicate: &dyn Fn(&CatalogEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let event = service.recv_event_timeout(Duration::from_millis(100));
                if let Ok(event) = event {
                    if predicate(&event) {
                        break event;
                    }
                }
                assert!(Instant::now() < deadline, "catalog event deadline");
            }
        };
        service
            .submit(CatalogCommand::Register {
                source_id: source.clone(),
                canonical_root: root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive(&|e| matches!(e, CatalogEvent::Registered { .. }));
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: source.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        let CatalogEvent::AuditCommitted { snapshot, .. } =
            receive(&|e| matches!(e, CatalogEvent::AuditCommitted { .. }))
        else {
            unreachable!()
        };
        service
            .submit(CatalogCommand::RequestFeatureAnalysis {
                source_id: source,
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 1,
                relative_path: "tone.wav".into(),
                cancellation: FeatureAnalysisCancellation::new(),
            })
            .unwrap();
        let CatalogEvent::FeatureAnalysisState { state, .. } = receive(&|e| {
            matches!(
                e,
                CatalogEvent::FeatureAnalysisState {
                    state: FeatureAnalysisState::Ready { .. }
                        | FeatureAnalysisState::Blocked { .. },
                    ..
                }
            )
        }) else {
            unreachable!()
        };
        assert!(
            matches!(state, FeatureAnalysisState::Ready { .. }),
            "{state:?}"
        );
        service.shutdown();
        state
    }

    #[gpui::test]
    fn explicit_states_and_old_published_ready_never_claim_current_completion(
        cx: &mut TestAppContext,
    ) {
        let ready = published_ready();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            for (state, phase, label, retry) in [
                (
                    FeatureAnalysisState::Queued,
                    FeatureJobState::Queued,
                    "Queued",
                    false,
                ),
                (
                    FeatureAnalysisState::Running,
                    FeatureJobState::Running,
                    "Running",
                    false,
                ),
                (
                    FeatureAnalysisState::Blocked {
                        reason: Blocker::InputChanged,
                    },
                    FeatureJobState::Blocked,
                    "Blocked",
                    true,
                ),
                (
                    FeatureAnalysisState::Blocked {
                        reason: Blocker::UnsupportedPlatform,
                    },
                    FeatureJobState::Blocked,
                    "Blocked",
                    false,
                ),
                (
                    ready.clone(),
                    FeatureJobState::Ready,
                    "Selected file ready",
                    false,
                ),
            ] {
                install(view, state);
                let row = view.feature_job_row().unwrap();
                assert_eq!(row.feature_state, Some(phase));
                assert!(row.state_label().starts_with(label));
                assert_eq!(row.can_retry, retry);
                assert_eq!(row.completed, phase == FeatureJobState::Ready);
            }
            // A real new owner attempt cancels the previous token before an audit.
            view.feature_analysis
                .pending
                .as_ref()
                .unwrap()
                .cancellation
                .cancel();
            let mut context = view.catalog_metadata.context().unwrap();
            context.revision += 1;
            view.catalog_metadata.install_projection(context);
            let outdated = view.feature_job_row().unwrap();
            assert_eq!(
                outdated.feature_state,
                Some(FeatureJobState::NeedsRevalidation)
            );
            assert!(!outdated.completed);
            assert!(outdated.can_retry);
            assert!(outdated.blocked_label().unwrap().contains("retry"));
            assert_eq!(snapshot(view, cx).active_count(), 0);
            assert_eq!(snapshot(view, cx).status_label(), "Job needs attention");
            let (source, lifecycle, revision, path) = view.feature_retry_target().unwrap();
            assert_eq!(source, SourceId::from_string("source"));
            assert_eq!((lifecycle, revision), (2, 4));
            assert_eq!(path, view.feature_analysis.pending.as_ref().unwrap().path);
            let event = CatalogEvent::FeatureAnalysisState {
                source_id: source,
                lifecycle_generation: 2,
                expected_revision: 3,
                request_id: 4,
                relative_path: path,
                state: ready,
            };
            apply_catalog_event(view, &event, cx);
            assert_eq!(view.feature_job_row(), Some(outdated.clone()));
            view.retry_current_job(cx); // Missing catalog owner rejects actual command admission.
            assert_eq!(view.feature_job_row(), Some(outdated));
            assert!(
                view.job_action_error
                    .as_ref()
                    .unwrap()
                    .contains("unavailable")
            );
        });
    }

    #[gpui::test]
    fn revalidation_requires_same_live_audited_owner_and_monotonic_revision(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            for mismatch in 0..5 {
                install(view, FeatureAnalysisState::Running);
                let mut context = view.catalog_metadata.context().unwrap();
                context.revision = match mismatch {
                    0 => 2,
                    4 => 3,
                    _ => 4,
                };
                view.catalog_metadata.install_projection(context);
                match mismatch {
                    1 => view.catalog_activation.as_mut().unwrap().audit_finished = false,
                    2 => {
                        view.catalog_activation
                            .as_mut()
                            .unwrap()
                            .lifecycle_generation += 1
                    }
                    3 => view.root = Some("/replacement".into()),
                    4 => view
                        .feature_analysis
                        .pending
                        .as_ref()
                        .unwrap()
                        .cancellation
                        .cancel(),
                    _ => {}
                }
                assert!(view.feature_job_row().is_none());
                assert!(view.feature_retry_target().is_none());
                view.retry_current_job(cx);
                assert_eq!(view.feature_analysis.next_request, 4);
            }
        });
    }

    #[gpui::test]
    fn unreadable_or_changed_input_remains_ambiguous_and_preparation_indeterminate(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _cx| {
            install(
                view,
                FeatureAnalysisState::Blocked {
                    reason: Blocker::InputChanged,
                },
            );
            let row = view.feature_job_row().unwrap();
            assert_eq!(row.feature_state, Some(FeatureJobState::Blocked));
            assert!(
                row.blocked_label()
                    .unwrap()
                    .contains("changed or could not be read")
            );
            assert!(!row.completed);
            install(view, FeatureAnalysisState::Queued);
            view.scan_cancel = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )));
            let jobs = snapshot(view, _cx);
            assert_eq!(jobs.status_label(), "2 jobs active");
            assert_eq!(jobs.rows[0].feature_state, Some(FeatureJobState::Queued));
            view.scan_cancel = None;
            view.feature_analysis.pending.as_mut().unwrap().state = None;
            let row = view.feature_job_row().unwrap();
            assert_eq!(row.feature_state, Some(FeatureJobState::Preparing));
            assert!(row.state_label().contains("time remaining unknown"));
            assert!(!row.completed && !row.can_retry);
        });
    }

    #[gpui::test]
    fn queued_and_revalidation_rows_keep_retry_visible_in_compact_job_details(
        cx: &mut TestAppContext,
    ) {
        use gpui::{AvailableSpace, point, px, size};
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        for (width, height) in [(960.0, 560.0), (1280.0, 800.0)] {
            cx.simulate_resize(size(px(width), px(height)));
            for outdated in [false, true] {
                browser.update(cx, |view, _| {
                    install(view, FeatureAnalysisState::Queued);
                    if outdated {
                        let mut context = view.catalog_metadata.context().unwrap();
                        context.revision += 1;
                        view.catalog_metadata.install_projection(context);
                    }
                    view.job_details_open = true;
                });
                cx.draw(
                    point(px(0.0), px(0.0)),
                    size(
                        AvailableSpace::Definite(px(width)),
                        AvailableSpace::Definite(px(height)),
                    ),
                    |_, _| browser.clone(),
                );
                let panel = cx.debug_bounds("status-job-details-panel").unwrap();
                let control = cx.debug_bounds("status-job-details").unwrap();
                assert!(panel.left() >= px(0.0) && panel.right() <= px(width));
                assert!(panel.top() >= px(0.0) && panel.bottom() <= control.top());
                if outdated {
                    let retry = cx.debug_bounds("status-job-retry").unwrap();
                    assert!(retry.left() >= panel.left() && retry.right() <= panel.right());
                    assert!(retry.top() >= panel.top() && retry.bottom() <= panel.bottom());
                }
            }
        }
    }
}
