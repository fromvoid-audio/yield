#[cfg(test)]
mod fixtures {
    use super::super::*;
    use std::{sync::Arc, time::Instant};
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use yield_session::{CatalogCommand, CatalogEvent, CatalogStatus};

    use crate::browser::{
        SourceSession, catalog::CatalogActivation, catalog_metadata::CatalogMetadataContext,
        source_registry::SourceRegistryGlobal,
    };

    struct ControlsFixture(gpui::Entity<Browser>);

    impl gpui::Render for ControlsFixture {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let theme = crate::theme::active(cx).clone();
            self.0.update(cx, |browser, cx| {
                render_collection_history(browser, theme, cx).into_any_element()
            })
        }
    }

    fn draw_controls(fixture: &gpui::Entity<ControlsFixture>, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            gpui::point(px(0.0), px(0.0)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(240.0)),
                gpui::AvailableSpace::Definite(px(100.0)),
            ),
            |_, _| fixture.clone(),
        );
    }

    fn next_event(
        mut drain: impl FnMut(&mut Vec<CatalogEvent>),
        predicate: impl Fn(&CatalogEvent) -> bool,
    ) -> CatalogEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut events = Vec::new();
            drain(&mut events);
            if let Some(event) = events.into_iter().find(&predicate) {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "real catalog owner did not return terminal event"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn write_wav(path: &Path) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        std::fs::write(path, bytes).expect("write WAV");
    }

    #[gpui::test]
    fn collection_history_real_gpui_owner_commit_undo_redo_and_restart(
        cx: &mut gpui::TestAppContext,
    ) {
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let config = tempfile::tempdir().expect("config base");
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-development");
        let source_root = tempfile::tempdir().expect("source root");
        let root = source_root.path().canonicalize().unwrap();
        write_wav(&root.join("one.wav"));
        let source_id = SourceId::from_string("collection-history-actual-owner");
        cx.set_global(crate::theme::ActiveTheme::default());
        let initial_guard = WritableProfileGuard::acquire_current().expect("fixture profile guard");
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(
            yield_catalog::CatalogService::start(&initial_guard).expect("actual retained catalog"),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status =
                cx.update_global::<SourceRegistryGlobal, _>(|global, _| global.catalog_status());
            if status == CatalogStatus::Ready {
                break;
            }
            assert!(
                matches!(status, CatalogStatus::Loading),
                "actual owner failed: {status:?}"
            );
            assert!(Instant::now() < deadline, "catalog startup deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
            global.submit_catalog(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: root.clone(),
                lifecycle_generation: 1,
            })
        })
        .expect("actual register admission");
        next_event(
            |events| {
                cx.update(|app| {
                    app.global::<SourceRegistryGlobal>()
                        .drain_catalog_events(events);
                });
            },
            |event| matches!(event, CatalogEvent::Registered { .. }),
        );
        cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
            global.submit_catalog(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
        })
        .expect("actual audit admission");
        let CatalogEvent::AuditCommitted { snapshot, .. } = next_event(
            |events| {
                cx.update(|app| {
                    app.global::<SourceRegistryGlobal>()
                        .drain_catalog_events(events);
                });
            },
            |event| matches!(event, CatalogEvent::AuditCommitted { .. }),
        ) else {
            unreachable!()
        };
        let metadata = &snapshot.curation_metadata[Path::new("one.wav")];
        let entry = AudioFile {
            path: root.join("one.wav"),
            relative_path: "one.wav".into(),
            byte_len: metadata.file_size,
            duration_seconds: Some(1.0 / 8000.0),
            sample_rate: Some(8000),
            playback_type: None,
            curation: Some(curation_from_metadata(metadata).expect("audited curation")),
        };
        let original = entry.curation.unwrap();
        let original_id = entry.stable_id();
        let (fixture, cx) =
            cx.add_window_view(|_, cx| ControlsFixture(cx.new(|cx| Browser::new(cx))));
        let browser = cx.update(|_, app| fixture.read(app).0.clone());
        cx.simulate_resize(gpui::size(px(240.0), px(100.0)));
        let viewport_offset = gpui::point(px(0.0), px(-7.0));
        browser.update(cx, |view, _| {
            view.source_registry_task.take();
            view.preferences_task.take();
            let mut source = SourceSession::new(root.clone());
            source.source_id = Some(source_id.clone());
            source.catalog_lifecycle_generation = Some(1);
            source.entries = vec![entry.clone()];
            view.source_sessions = vec![source];
            view.active_source = Some(0);
            view.root = Some(root.clone());
            view.entries = vec![entry.clone()];
            view.view_model.set_entries(&view.entries);
            view.selected = Some(0);
            view.selection.focus_pointer(&original_id);
            view.list_scroll_handle
                .0
                .borrow()
                .base_handle
                .set_offset(viewport_offset);
            view.catalog_activation = Some(CatalogActivation {
                source_id: source_id.clone(),
                root: root.clone(),
                lifecycle_generation: 1,
                registered: true,
                registration_admitted: true,
                root_identity: Some(snapshot.root_identity.clone()),
                audit_finished: true,
                audit_error: None,
                audit_retryable: false,
                force_reload: false,
                fallback_to_scan: false,
            });
            view.catalog_metadata
                .install_projection(CatalogMetadataContext {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                    root: root.clone(),
                    root_identity: snapshot.root_identity.clone(),
                    directory_generation: snapshot.directory_generation,
                    revision: snapshot.committed_revision,
                    entry_indices: Arc::new(HashMap::from([(PathBuf::from("one.wav"), 0)])),
                });
            view.curation_state.install_projection(
                CurationLifecycleKey {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                },
                snapshot.root_identity.clone(),
                snapshot.directory_generation,
                snapshot.committed_revision,
                &view.entries,
            );
        });
        // Every transition uses the active Browser method, real retained catalog admission, SQLite
        // commit and terminal owner event. No constructed success receipt enters this fixture.
        for (step, expected_member) in [true, false, true, false, true].into_iter().enumerate() {
            if step == 0 || step == 4 {
                browser.update(cx, |view, cx| {
                    view.submit_curation_action(
                        CurationAction::ToggleCollection { slot: 0 },
                        None,
                        cx,
                    );
                });
            } else {
                browser.update(cx, |view, _| {
                    view.selection.clear();
                    view.browser_focus_detached = true;
                    view.list_scroll_handle
                        .0
                        .borrow()
                        .base_handle
                        .set_offset(gpui::point(px(0.0), px(-33.0)));
                });
                draw_controls(&fixture, cx);
                let id = if step == 2 {
                    "collection-history-redo"
                } else {
                    "collection-history-undo"
                };
                let bounds = cx
                    .debug_bounds(id)
                    .expect("visible collection history control");
                cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            }
            browser.update(cx, |view, _| {
                view.source_registry_task.take();
                assert!(view.curation_state.collection_history.pending.is_some());
                if step == 0 {
                    assert!(view.curation_state.collection_history.undo.is_empty());
                }
            });
            let event = next_event(
                |events| {
                    cx.update(|_, app| {
                        app.global::<SourceRegistryGlobal>()
                            .drain_catalog_events(events);
                    });
                },
                |event| matches!(event, CatalogEvent::CurationOperationCompleted { .. }),
            );
            browser.update(cx, |view, cx| {
                assert!(view.apply_curation_event(&event, cx));
                let history = &view.curation_state.collection_history;
                assert!(history.pending.is_none());
                assert_eq!(
                    view.entries[0].curation.unwrap().has_collection(0),
                    expected_member
                );
                assert_eq!(
                    view.entries[0].curation.unwrap().rating(),
                    original.rating()
                );
                assert_eq!(
                    view.entries[0].curation.unwrap().last_played_at(),
                    original.last_played_at()
                );
                assert_eq!(view.selection.anchor(), Some(&original_id));
                assert_eq!(view.browser_focus_id(), Some(original_id.clone()));
                assert_eq!(
                    view.list_scroll_handle.0.borrow().base_handle.offset(),
                    viewport_offset
                );
                if expected_member {
                    assert!(history.redo.is_empty());
                } else {
                    assert_eq!(history.redo.len(), 1);
                }
                let stack_sizes = (history.undo.len(), history.redo.len());
                assert!(view.apply_curation_event(&event, cx)); // duplicate delivery is drained once
                assert_eq!(
                    (
                        view.curation_state.collection_history.undo.len(),
                        view.curation_state.collection_history.redo.len()
                    ),
                    stack_sizes
                );
            });
        }
        // Losing the owner before undo admission preserves the settled stack and cannot report a commit.
        cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
        browser.update(cx, |view, cx| {
            let size = view.curation_state.collection_history.undo.len();
            view.submit_collection_history(Direction::Undo, cx);
            assert_eq!(view.curation_state.collection_history.undo.len(), size);
            assert!(view.curation_state.collection_history.pending.is_none());
        });
        // Reopen the actual retained database with a new catalog owner, proving membership persistence.
        drop(initial_guard);
        let guard = WritableProfileGuard::acquire_current().expect("restart profile guard");
        let service = yield_catalog::CatalogService::start(&guard).expect("restart catalog");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: root,
                lifecycle_generation: 2,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(
                service.try_recv_event(),
                Ok(CatalogEvent::Registered { .. })
            ) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 2,
            })
            .unwrap();
        loop {
            if let Ok(CatalogEvent::AuditCommitted { snapshot, .. }) = service.try_recv_event() {
                assert!(
                    snapshot.curation_metadata[Path::new("one.wav")]
                        .collections
                        .iter()
                        .any(|slot| slot.index() == 0)
                );
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        service.shutdown();
        // Metadata persistence is established. Undo history restart remains OPT-2115.
    }

    fn state() -> (CurationState, CurationLifecycleKey, PathBuf) {
        let key = CurationLifecycleKey {
            source_id: SourceId::from_string("history-contract"),
            lifecycle_generation: 1,
        };
        let path = PathBuf::from("one.wav");
        let entry = AudioFile {
            path: PathBuf::from("/source/one.wav"),
            relative_path: path.clone(),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: AudioFileCuration::from_parts(1, false, 0, Some(7), Some(8)),
        };
        let mut state = CurationState::default();
        state.install_projection(
            key.clone(),
            RootIdentity::from_bytes(b"root".to_vec()),
            2,
            10,
            &[entry],
        );
        (state, key, path)
    }

    fn admit(state: &mut CurationState, key: &CurationLifecycleKey, path: &PathBuf) -> u64 {
        let CurationPlan::Ready(plan) = state.plan_action(
            key,
            CurationAction::ToggleCollection { slot: 0 },
            std::slice::from_ref(path),
        ) else {
            panic!("collection plan");
        };
        let request = plan.request_id;
        assert!(state.accept_operation_with_presentation(key, plan, Some(Presentation::default())));
        request
    }

    fn outcome(initial: u64, paths: &[(&str, u8)]) -> CurationOutcome {
        let files = paths
            .iter()
            .map(|(path, mask)| {
                (
                    PathBuf::from(path),
                    AudioFileCuration::from_parts(1, false, *mask, Some(7), Some(8)).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let playback_types = files.keys().map(|path| (path.clone(), None)).collect();
        CurationOutcome {
            initial_revision: Some(initial),
            final_revision: Some(initial + 1),
            failure: None,
            committed_batches: vec![CurationReceipt {
                source_revision: initial + 1,
                root_identity: RootIdentity::from_bytes(b"root".to_vec()),
                directory_generation: 2,
                files,
                playback_types,
                valid: true,
            }],
        }
    }

    #[test]
    fn collection_history_rejects_incomplete_wrong_bit_and_stale_publication_receipts() {
        for paths in [vec![], vec![("other.wav", 1)], vec![("one.wav", 0)]] {
            let (mut state, key, path) = state();
            let request = admit(&mut state, &key, &path);
            assert!(state.collection_history.undo.is_empty());
            state.apply_outcome(key, request, outcome(10, &paths));
            assert!(state.collection_history.undo.is_empty());
            assert!(state.collection_history.redo.is_empty());
        }
        let original = outcome(10, &[("one.wav", 1)]);
        let mut changed = Vec::new();
        let mut receipt = original.clone();
        receipt.committed_batches[0].root_identity =
            RootIdentity::from_bytes(b"replacement".to_vec());
        changed.push(receipt);
        let mut receipt = original.clone();
        receipt.committed_batches[0].directory_generation += 1;
        changed.push(receipt);
        let mut receipt = original.clone();
        receipt.initial_revision = Some(9);
        changed.push(receipt);
        let mut receipt = original.clone();
        receipt.final_revision = Some(12);
        changed.push(receipt);
        let mut receipt = original.clone();
        receipt.committed_batches[0].valid = false;
        changed.push(receipt);
        let mut receipt = original.clone();
        receipt
            .committed_batches
            .push(receipt.committed_batches[0].clone());
        changed.push(receipt);
        for receipt in changed {
            let (mut state, key, path) = state();
            let request = admit(&mut state, &key, &path);
            state.apply_outcome(key, request, receipt);
            assert!(state.collection_history.pending.is_none());
            assert!(state.collection_history.undo.is_empty());
            assert!(state.collection_history.redo.is_empty());
        }
        let (mut state, key, path) = state();
        let request = admit(&mut state, &key, &path);
        let stale_key = CurationLifecycleKey {
            lifecycle_generation: 2,
            ..key.clone()
        };
        assert!(
            state
                .apply_outcome(stale_key, request, outcome(10, &[("one.wav", 1)]))
                .is_none()
        );
        assert!(state.collection_history.undo.is_empty());
        state.apply_outcome_with_projection(key, request, outcome(10, &[("one.wav", 1)]), false);
        assert!(state.collection_history.pending.is_none());
        assert!(state.collection_history.undo.is_empty());
    }

    #[test]
    fn collection_history_noop_failure_and_intervening_commit_keep_stack_honest() {
        let (mut state, key, path) = state();
        let request = admit(&mut state, &key, &path);
        state.apply_outcome(key.clone(), request, outcome(10, &[("one.wav", 1)]));
        assert_eq!(state.collection_history.undo.len(), 1);
        assert!(matches!(
            state.plan_action(
                &key,
                CurationAction::RemoveCollection { slot: 1 },
                std::slice::from_ref(&path)
            ),
            CurationPlan::NoChange { .. }
        ));
        assert_eq!(state.collection_history.undo.len(), 1);
        let plan = state
            .collection_history
            .plan(
                &key,
                &state.lineages[&key],
                state.next_request_id(),
                Direction::Undo,
            )
            .unwrap();
        let request = plan.request_id;
        assert!(matches!(
            plan.batches[0][0],
            SourceCurationUpdate::RemoveCollection { .. }
        ));
        state.accept_operation_with_presentation(&key, plan, None);
        state.apply_outcome(
            key.clone(),
            request,
            CurationOutcome {
                initial_revision: Some(11),
                final_revision: Some(11),
                committed_batches: vec![],
                failure: Some("writer rejected before commit".into()),
            },
        );
        assert_eq!(state.collection_history.undo.len(), 1);
        assert!(state.collection_history.redo.is_empty());
        assert!(
            !state
                .collection_history
                .ready(&key, &state.lineages[&key], Direction::Undo)
        );
        // A committed untracked metadata write is not a history continuation grant.
        state
            .collection_history
            .complete(&key, 900, &outcome(11, &[("one.wav", 1)]), true);
        assert!(state.collection_history.undo.is_empty());
    }

    #[test]
    fn collection_history_bounds_settled_and_pending_payload_and_retired_context() {
        let (mut state, key, path) = state();
        for index in 0..(MAX_RECORDS + 10) {
            let initial = state.lineages[&key].revision;
            let request = admit(&mut state, &key, &path);
            assert!(
                state.collection_history.undo.len()
                    + usize::from(state.collection_history.pending.is_some())
                    <= MAX_RECORDS
            );
            assert!(state.collection_history.bytes() <= MAX_BYTES);
            state.apply_outcome(
                key.clone(),
                request,
                outcome(initial, &[("one.wav", u8::from(index % 2 == 0))]),
            );
        }
        assert_eq!(state.collection_history.undo.len(), MAX_RECORDS);
        let mut oversized = Presentation::default();
        let long = AudioFile {
            path: PathBuf::from("x".repeat(MAX_BYTES / 3 + 1)),
            relative_path: "long.wav".into(),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
        .stable_id();
        oversized.selection.focus_pointer(&long);
        let CurationPlan::Ready(plan) = state.plan_action(
            &key,
            CurationAction::ToggleCollection { slot: 0 },
            std::slice::from_ref(&path),
        ) else {
            panic!("changing plan");
        };
        let initial = state.lineages[&key].revision;
        let request = plan.request_id;
        state.accept_operation_with_presentation(&key, plan, Some(oversized));
        assert!(state.collection_history.pending.is_none());
        state.apply_outcome(key.clone(), request, outcome(initial, &[("one.wav", 1)]));
        assert!(state.collection_history.undo.is_empty());
        assert!(state.status_for(&key).unwrap().contains("Undo unavailable"));
    }

    #[test]
    fn collection_history_retired_pending_context_releases_presentation_and_drains_late_outcome() {
        let (mut state, key, path) = state();
        let request = admit(&mut state, &key, &path);
        assert!(state.collection_history.pending.is_some());
        state.prune_completed_except(None);
        assert!(state.collection_history.pending.is_none());
        assert!(state.status_for(&key).unwrap().contains("unconfirmed"));
        assert!(
            state
                .apply_outcome(key.clone(), request, outcome(10, &[("one.wav", 1)]))
                .is_some()
        );
        assert!(state.collection_history.undo.is_empty());
        assert!(state.collection_history.redo.is_empty());
        state.prune_completed_except(None);
        assert!(!state.lineages.contains_key(&key));
    }

    #[test]
    fn collection_history_late_retired_completion_preserves_new_source_history() {
        for projectable in [true, false] {
            let (mut state, old_key, path) = self::state();
            let old_request = admit(&mut state, &old_key, &path);
            state.prune_completed_except(None);
            let (mut fresh, fresh_key, fresh_path) = self::state();
            let new_key = CurationLifecycleKey {
                source_id: SourceId::from_string("new-source"),
                ..fresh_key.clone()
            };
            state
                .lineages
                .insert(new_key.clone(), fresh.lineages.remove(&fresh_key).unwrap());
            let new_request = admit(&mut state, &new_key, &fresh_path);
            state.apply_outcome(new_key.clone(), new_request, outcome(10, &[("one.wav", 1)]));
            assert_eq!(state.collection_history.undo.len(), 1);
            state.apply_outcome_with_projection(
                old_key.clone(),
                old_request,
                outcome(10, &[("one.wav", 1)]),
                projectable,
            );
            state.prune_completed_except(Some(&new_key));
            assert!(!state.lineages.contains_key(&old_key));
            assert_eq!(state.collection_history.undo.len(), 1);
            assert!(state.collection_history.ready(
                &new_key,
                &state.lineages[&new_key],
                Direction::Undo
            ));
        }
    }
}
