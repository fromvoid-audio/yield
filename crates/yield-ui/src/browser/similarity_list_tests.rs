#[cfg(test)]
mod fixtures {
    use super::super::*;
    use gpui::TestAppContext;
    use yield_library::sample_sources::reconciliation::RootIdentity;

    fn install(browser: &mut Browser) {
        super::super::super::analysis::tests::install(
            browser,
            yield_session::FeatureAnalysisState::Queued,
        );
        browser.entries = ["anchor.wav", "same.wav", "missing.wav"]
            .into_iter()
            .map(|path| yield_core::AudioFile {
                path: browser.root.as_ref().unwrap().join(path),
                relative_path: path.into(),
                byte_len: 64,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            })
            .collect();
        browser.view_model = yield_core::BrowserViewModel::new(&browser.entries);
        let mut context = browser.catalog_metadata.context().unwrap();
        context.entry_indices = Arc::new(
            browser
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.relative_path.clone(), index))
                .collect(),
        );
        browser.catalog_metadata.install_projection(context);
    }
    #[gpui::test]
    fn similarity_anchor_toggle_and_aspect_change_cancel_old_requests(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(view);
            view.toggle_similarity_anchor("anchor.wav".into(), cx);
            let old = view.similarity_list.active.as_ref().unwrap();
            assert!(old.terminal); // Actual missing session rejects admission.
            assert!(old.result.is_none());
            assert!(old.status.contains("unavailable"));
            assert_eq!(old.request.candidates().len(), 2);
            let cancellation = old.cancellation.clone();
            let generation = old.request.generation();
            view.request_similarity("anchor.wav".into(), SimilarityAspect::Timbre, cx);
            assert!(cancellation.is_cancelled());
            let current = view.similarity_list.active.as_ref().unwrap();
            assert!(current.request.generation() > generation);
            assert_eq!(current.request.aspect(), SimilarityAspect::Timbre);
            let cancellation = current.cancellation.clone();
            view.toggle_similarity_anchor("anchor.wav".into(), cx);
            assert!(view.similarity_list.active.is_none());
            assert!(cancellation.is_cancelled());
        });
    }
    #[gpui::test]
    fn similarity_rejects_source_query_revision_and_mapping_changes_before_render(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(view);
            let changes: [fn(&mut Browser); 8] = [
                |v| {
                    v.root = Some("/different".into());
                },
                |v| {
                    v.catalog_activation.as_mut().unwrap().lifecycle_generation += 1;
                },
                |v| {
                    v.catalog_next_generation += 1;
                },
                |v| {
                    v.scan_generation += 1;
                },
                |v| {
                    v.view_model.set_query(&v.entries, "same");
                },
                |v| {
                    let mut c = v.catalog_metadata.context().unwrap();
                    c.revision += 1;
                    v.catalog_metadata.install_projection(c);
                },
                |v| {
                    let mut c = v.catalog_metadata.context().unwrap();
                    c.root_identity = RootIdentity::from_bytes(b"changed".to_vec());
                    v.catalog_metadata.install_projection(c);
                },
                |v| {
                    let mut c = v.catalog_metadata.context().unwrap();
                    c.entry_indices = Arc::new((*c.entry_indices).clone());
                    v.catalog_metadata.install_projection(c);
                },
            ];
            for change in changes {
                install(view);
                view.request_similarity("anchor.wav".into(), SimilarityAspect::Spectrum, cx);
                let token = view
                    .similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .cancellation
                    .clone();
                change(view);
                assert!(
                    !view
                        .similarity_list
                        .active
                        .as_ref()
                        .unwrap()
                        .is_current(view)
                );
                invalidate_if_stale(view);
                assert!(view.similarity_list.active.is_none());
                assert!(token.is_cancelled());
            }
        });
    }
    #[gpui::test]
    fn similarity_admission_is_bounded_to_visible_candidates(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            install(view);
            view.entries
                .extend((0..2000).map(|i| yield_core::AudioFile {
                    path: view.root.as_ref().unwrap().join(format!("{i}.wav")),
                    relative_path: format!("{i}.wav").into(),
                    byte_len: 1,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                }));
            view.view_model.set_entries(&view.entries);
            view.request_similarity("anchor.wav".into(), SimilarityAspect::Overall, cx);
            assert_eq!(
                view.similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .request
                    .candidates()
                    .len(),
                64
            );
            view.similarity_list.cancel();
            view.view_model.set_query(&view.entries, "same");
            view.request_similarity("anchor.wav".into(), SimilarityAspect::Overall, cx);
            assert_eq!(
                view.similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .request
                    .candidates(),
                &["same.wav"]
            );
        });
    }
    use super::super::super::{
        SourceSession, catalog::CatalogActivation, source_registry::SourceRegistryGlobal,
    };
    use std::time::{Duration, Instant};
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    struct OwnerFixture {
        guard: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _base: ConfigBaseGuard,
        root: tempfile::TempDir,
        _config: tempfile::TempDir,
    }
    fn owner() -> (
        OwnerFixture,
        yield_catalog::CatalogService,
        yield_catalog::CatalogSnapshot,
    ) {
        let config = tempfile::tempdir().unwrap();
        let base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let profile = PersistenceProfileGuard::named("similarity-gpui-owner");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let frames = 4000u32;
        let bytes = frames * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes.to_le_bytes());
        for i in 0..frames {
            wav.extend_from_slice(&(((i as f64 / 16.).sin() * 12000.) as i16).to_le_bytes());
        }
        for name in ["anchor.wav", "same.wav", "missing.wav"] {
            std::fs::write(root.path().join(name), &wav).unwrap();
        }
        let service = yield_catalog::CatalogService::start(&guard).unwrap();
        let id = yield_library::sample_sources::SourceId::from_string("similarity-gpui-owner");
        service
            .submit(CatalogCommand::Register {
                source_id: id.clone(),
                canonical_root: root.path().into(),
                lifecycle_generation: 1,
            })
            .unwrap();
        next_owner(&service, |e| matches!(e, CatalogEvent::Registered { .. }));
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: id.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        let CatalogEvent::AuditCommitted { snapshot } = next_owner(&service, |e| {
            matches!(e, CatalogEvent::AuditCommitted { .. })
        }) else {
            panic!()
        };
        for (index, name) in ["anchor.wav", "same.wav"].into_iter().enumerate() {
            service
                .submit(CatalogCommand::RequestFeatureAnalysis {
                    source_id: id.clone(),
                    lifecycle_generation: 1,
                    expected_revision: snapshot.committed_revision,
                    request_id: index as u64 + 1,
                    relative_path: name.into(),
                    cancellation: yield_session::FeatureAnalysisCancellation::new(),
                })
                .unwrap();
            let event = next_owner(&service, |e| {
                matches!(
                    e,
                    CatalogEvent::FeatureAnalysisState {
                        state: yield_session::FeatureAnalysisState::Ready { .. }
                            | yield_session::FeatureAnalysisState::Blocked { .. },
                        ..
                    }
                )
            });
            assert!(
                matches!(
                    event,
                    CatalogEvent::FeatureAnalysisState {
                        state: yield_session::FeatureAnalysisState::Ready { .. },
                        ..
                    }
                ),
                "{event:?}"
            );
        }
        (
            OwnerFixture {
                guard,
                _profile: profile,
                _base: base,
                root,
                _config: config,
            },
            service,
            snapshot,
        )
    }
    fn next_owner(
        service: &yield_catalog::CatalogService,
        matches: impl Fn(&CatalogEvent) -> bool,
    ) -> CatalogEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(event) = service.try_recv_event() {
                if matches(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "owner completion timeout");
            std::thread::yield_now();
        }
    }
    fn install_owned(
        view: &mut Browser,
        fixture: &OwnerFixture,
        snapshot: &yield_catalog::CatalogSnapshot,
    ) {
        let root = fixture.root.path().to_path_buf();
        let mut session = SourceSession::new(root.clone());
        session.source_id = Some(snapshot.source_id.clone());
        view.source_sessions = vec![session];
        view.active_source = Some(0);
        view.root = Some(root.clone());
        view.source_registry_loaded = true;
        view.catalog_activation = Some(CatalogActivation {
            source_id: snapshot.source_id.clone(),
            root: root.clone(),
            lifecycle_generation: snapshot.lifecycle_generation,
            registered: true,
            registration_admitted: true,
            root_identity: Some(snapshot.root_identity.clone()),
            audit_finished: true,
            audit_error: None,
            audit_retryable: false,
            force_reload: false,
            fallback_to_scan: false,
        });
        view.entries = snapshot
            .manifest
            .iter()
            .map(|entry| yield_core::AudioFile {
                path: root.join(&entry.relative_path),
                relative_path: entry.relative_path.clone(),
                byte_len: entry.file_size,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            })
            .collect();
        view.view_model = yield_core::BrowserViewModel::new(&view.entries);
        view.catalog_metadata
            .install_projection(CatalogMetadataContext {
                source_id: snapshot.source_id.clone(),
                lifecycle_generation: snapshot.lifecycle_generation,
                root,
                root_identity: snapshot.root_identity.clone(),
                directory_generation: snapshot.directory_generation,
                revision: snapshot.committed_revision,
                entry_indices: Arc::new(
                    view.entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| (e.relative_path.clone(), i))
                        .collect(),
                ),
            });
    }
    struct Controls(gpui::Entity<Browser>);
    impl gpui::Render for Controls {
        fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::active(cx).clone();
            self.0.update(cx, |view, cx| {
                div()
                    .relative()
                    .size_full()
                    .child(anchor_button(
                        view,
                        std::path::Path::new("anchor.wav"),
                        0,
                        cx,
                    ))
                    .child(render(view, &theme, cx))
            })
        }
    }
    fn draw_controls(controls: &gpui::Entity<Controls>, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(600.)),
                gpui::AvailableSpace::Definite(px(600.)),
            ),
            |_, _| controls.clone(),
        );
    }
    fn next_delivery(
        cx: &mut gpui::VisualTestContext,
        _generation: u64,
    ) -> yield_session::CatalogDelivery {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let deliveries = cx.update(|_, app| {
                let mut result = Vec::new();
                app.global::<SourceRegistryGlobal>()
                    .drain_catalog_deliveries(&mut result);
                result
            });
            if let Some(delivery) = deliveries.into_iter().next() {
                return delivery;
            }
            assert!(Instant::now() < deadline, "UI owner delivery timeout");
            std::thread::yield_now();
        }
    }
    #[gpui::test]
    fn actual_similarity_controls_admit_owner_scores_render_and_reject_late_aspects(
        cx: &mut TestAppContext,
    ) {
        let (fixture, service, snapshot) = owner();
        cx.set_global(crate::theme::ActiveTheme::default());
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        let (controls, cx) = cx.add_window_view(|_, cx| Controls(cx.new(|cx| Browser::new(cx))));
        let browser = cx.update(|_, app| controls.read(app).0.clone());
        browser.update(cx, |view, _| install_owned(view, &fixture, &snapshot));
        draw_controls(&controls, cx);
        let anchor = cx.debug_bounds("sample-similarity-anchor-0").unwrap();
        cx.simulate_click(anchor.center(), gpui::Modifiers::none());
        let (generation, token) = browser.read_with(cx, |view, _| {
            let p = view.similarity_list.active.as_ref().unwrap();
            assert!(!p.terminal);
            (p.request.generation(), p.cancellation.clone())
        });
        let old_delivery = next_delivery(cx, generation);
        draw_controls(&controls, cx);
        let aspect = cx.debug_bounds("sample-similarity-aspect-1").unwrap();
        cx.simulate_click(aspect.center(), gpui::Modifiers::none());
        assert!(token.is_cancelled());
        browser.update(cx, |view, cx| {
            assert!(!view.apply_catalog_delivery(old_delivery, cx));
        });
        let generation = browser.read_with(cx, |view, _| {
            view.similarity_list
                .active
                .as_ref()
                .unwrap()
                .request
                .generation()
        });
        let delivery = next_delivery(cx, generation);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(delivery, cx));
            let p = view.similarity_list.active.as_ref().unwrap();
            assert!(p.terminal && p.is_current(view));
            assert_eq!(p.request.aspect(), SimilarityAspect::Spectrum);
            let result = p.result.as_ref().unwrap();
            assert_eq!(result.rows().len(), 2);
            assert!(result.rows().iter().any(
                |r| matches!(r.outcome(), SimilarityCandidateOutcome::Scored(x) if *x > 0.99)
            ));
            assert!(
                result
                    .rows()
                    .iter()
                    .any(|r| matches!(r.outcome(), SimilarityCandidateOutcome::Unavailable))
            );
        });
        draw_controls(&controls, cx);
        assert!(cx.debug_bounds("sample-similarity-row-0").is_some());
        assert!(cx.debug_bounds("sample-similarity-row-1").is_some());
        browser.update(cx, |view, cx| {
            view.view_model.set_query(&view.entries, "same");
            cx.notify();
        });
        browser.read_with(cx, |view, _| {
            assert!(
                !view
                    .similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .is_current(view)
            );
        });
        // GPUI keeps debug bounds in its frame cache. A new window has no old selectors.
        let (fresh_controls, fresh_cx) = cx.add_window_view(|_, _| Controls(browser.clone()));
        draw_controls(&fresh_controls, fresh_cx);
        assert!(fresh_cx.debug_bounds("sample-similarity-row-0").is_none());
        fresh_cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
        assert!(fixture.guard.profile_root().exists());
    }
    #[gpui::test]
    fn similarity_requires_success_fence_and_hides_retired_owner_rows(cx: &mut TestAppContext) {
        let (fixture, service, snapshot) = owner();
        cx.set_global(crate::theme::ActiveTheme::default());
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        let (controls, cx) = cx.add_window_view(|_, cx| Controls(cx.new(|cx| Browser::new(cx))));
        let browser = cx.update(|_, app| controls.read(app).0.clone());
        browser.update(cx, |view, cx| {
            install_owned(view, &fixture, &snapshot);
            view.request_similarity("missing.wav".into(), SimilarityAspect::Overall, cx);
        });
        let generation = browser.read_with(cx, |view, _| {
            view.similarity_list
                .active
                .as_ref()
                .unwrap()
                .request
                .generation()
        });
        let delivery = next_delivery(cx, generation);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(delivery, cx));
            let p = view.similarity_list.active.as_ref().unwrap();
            assert!(p.terminal && p.result.is_none() && p.fence.is_none());
            assert!(p.status.contains("analyze the anchor"));
            view.request_similarity("anchor.wav".into(), SimilarityAspect::Amplitude, cx);
        });
        let generation = browser.read_with(cx, |view, _| {
            view.similarity_list
                .active
                .as_ref()
                .unwrap()
                .request
                .generation()
        });
        let (mut event, fence) = next_delivery(cx, generation).into_parts_if_live().unwrap();
        assert!(fence.as_ref().unwrap().is_live());
        browser.update(cx, |view, cx| {
            assert!(apply_catalog_event(view, &mut event, None, cx));
            assert!(
                view.similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .result
                    .is_none()
            );
            assert!(apply_catalog_event(view, &mut event, fence, cx));
            assert!(
                view.similarity_list
                    .active
                    .as_ref()
                    .unwrap()
                    .result
                    .is_some()
            );
        });
        draw_controls(&controls, cx);
        assert!(cx.debug_bounds("sample-similarity-row-0").is_some());
        cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
        browser.read_with(cx, |view, _| {
            let p = view.similarity_list.active.as_ref().unwrap();
            assert!(!p.fence.as_ref().unwrap().is_live());
            assert!(!p.is_current(view));
        });
        let (fresh_controls, fresh_cx) = cx.add_window_view(|_, _| Controls(browser.clone()));
        draw_controls(&fresh_controls, fresh_cx);
        assert!(fresh_cx.debug_bounds("sample-similarity-row-0").is_none());
        browser.update(fresh_cx, |view, _| {
            invalidate_if_stale(view);
            assert!(view.similarity_list.active.is_none());
        });
    }
    #[gpui::test]
    fn captured_anchor_target_cannot_retarget_same_path_in_a_replaced_source(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (controls, cx) = cx.add_window_view(|_, cx| Controls(cx.new(|cx| Browser::new(cx))));
        let browser = cx.update(|_, app| controls.read(app).0.clone());
        browser.update(cx, |view, _| install(view));
        draw_controls(&controls, cx);
        let target = browser.read_with(cx, |view, _| {
            AnchorTarget::capture(view, std::path::Path::new("anchor.wav"), 0)
        });
        browser.update(cx, |view, _| {
            let other = yield_library::sample_sources::SourceId::from_string("replacement-source");
            view.source_sessions[0].source_id = Some(other.clone());
            view.catalog_activation.as_mut().unwrap().source_id = other.clone();
            let mut context = view.catalog_metadata.context().unwrap();
            context.source_id = other;
            view.catalog_metadata.install_projection(context);
        });
        // Inject the target captured by the prior frame into the actual activation handler.
        browser.update(cx, |view, cx| target.activate(view, cx));
        browser.read_with(cx, |view, _| {
            assert!(view.similarity_list.active.is_none());
            assert_eq!(view.similarity_list.next_generation, 0);
            assert_eq!(view.selected, None);
        });
    }
}
