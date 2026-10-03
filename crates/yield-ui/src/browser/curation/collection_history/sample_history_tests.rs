#[cfg(test)]
mod sample_history {
    use super::*;
    // Actual retained-owner GPUI sample history cases; no synthetic commit evidence.
    fn sample_history_actual(cx: &mut gpui::TestAppContext, count: usize, mode: &str) {
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let config = tempfile::tempdir().expect("config base");
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-development");
        let source_root = tempfile::tempdir().expect("source root");
        let root = source_root.path().canonicalize().unwrap();
        for index in 0..count {
            write_wav(&root.join(if index == 0 {
                "one.wav".to_string()
            } else {
                format!("{index:03}.wav")
            }));
        }
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
        // Seed only through genuine retained owner curation; no direct SQL fixture writes.
        let paths = snapshot
            .curation_metadata
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut seed = Vec::new();
        for path in &paths {
            let relative = RootRelativePath::try_from_path(path.clone()).unwrap();
            seed.push(SourceCurationUpdate::SetRating {
                path: relative.clone(),
                rating: Rating::new(3),
            });
            seed.push(SourceCurationUpdate::SetLocked {
                path: relative.clone(),
                locked: true,
            });
            seed.push(SourceCurationUpdate::SetLastPlayedAt {
                path: relative.clone(),
                played_at: Some(7),
            });
            seed.push(SourceCurationUpdate::AddCollection {
                path: relative,
                collection: SampleCollection::new(1).unwrap(),
            });
        }
        cx.update_global::<SourceRegistryGlobal, _>(|g, _| {
            g.submit_catalog(CatalogCommand::ApplyCurationOperation {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                fence: SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                root_identity: snapshot.root_identity.clone(),
                request_id: 1,
                batches: seed
                    .chunks(MAX_CURATION_UPDATES)
                    .map(|c| c.to_vec())
                    .collect(),
            })
        })
        .unwrap();
        let seed_event = next_event(
            |events| {
                cx.update(|app| {
                    app.global::<SourceRegistryGlobal>()
                        .drain_catalog_events(events)
                });
            },
            |e| matches!(e, CatalogEvent::CurationOperationCompleted { .. }),
        );
        let CatalogEvent::CurationOperationCompleted { outcome, .. } = seed_event else {
            unreachable!()
        };
        assert!(outcome.is_success());
        cx.update_global::<SourceRegistryGlobal, _>(|g, _| {
            g.submit_catalog(CatalogCommand::ReadSnapshot {
                source_id: source_id.clone(),
                expected_revision: outcome.final_revision(),
            })
        })
        .unwrap();
        let CatalogEvent::SnapshotReady { snapshot } = next_event(
            |events| {
                cx.update(|app| {
                    app.global::<SourceRegistryGlobal>()
                        .drain_catalog_events(events)
                });
            },
            |e| matches!(e, CatalogEvent::SnapshotReady { .. }),
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
        let entries = paths
            .iter()
            .map(|p| {
                let m = &snapshot.curation_metadata[p];
                AudioFile {
                    path: root.join(p),
                    relative_path: p.clone(),
                    curation: Some(curation_from_metadata(m).unwrap()),
                    ..entry.clone()
                }
            })
            .collect::<Vec<_>>();
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
            view.curation_state.next_request_id = 1;
            let mut source = SourceSession::new(root.clone());
            source.source_id = Some(source_id.clone());
            source.catalog_lifecycle_generation = Some(1);
            source.entries = entries.clone();
            view.source_sessions = vec![source];
            view.active_source = Some(0);
            view.root = Some(root.clone());
            view.entries = entries.clone();
            view.view_model.set_entries(&view.entries);
            view.selected = entries
                .iter()
                .position(|e| e.relative_path == Path::new("one.wav"));
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
                    entry_indices: Arc::new(
                        entries
                            .iter()
                            .enumerate()
                            .map(|(i, e)| (e.relative_path.clone(), i))
                            .collect(),
                    ),
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

        assert_eq!(original.rating().value(), 3);
        assert!(original.rating().locked());
        let key = CurationLifecycleKey {
            source_id: source_id.clone(),
            lifecycle_generation: 1,
        };
        let apply = |browser: &gpui::Entity<Browser>, cx: &mut gpui::VisualTestContext| {
            let event = browser_owner_event(browser, cx);
            browser.update(cx, |v, cx| {
                assert!(v.apply_curation_event(&event, cx));
            });
            event
        };
        let click = |browser: &gpui::Entity<Browser>,
                     fixture: &gpui::Entity<ControlsFixture>,
                     cx: &mut gpui::VisualTestContext,
                     direction| {
            draw_controls(fixture, cx);
            let id = if direction == Direction::Undo {
                "collection-history-undo"
            } else {
                "collection-history-redo"
            };
            let bounds = cx.debug_bounds(id).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            browser.update(cx, |v, _| {
                v.source_registry_task.take();
            });
        };
        if mode.starts_with("full_redo") {
            for _ in 0..MAX_RECORDS {
                browser.update(cx, |v, cx| {
                    v.submit_curation_action(
                        CurationAction::ToggleCollection { slot: 0 },
                        Some(paths.clone()),
                        cx,
                    );
                    v.source_registry_task.take();
                });
                apply(&browser, cx);
            }
            for _ in 0..MAX_RECORDS {
                click(&browser, &fixture, cx, Direction::Undo);
                apply(&browser, cx);
            }
            browser.update(cx, |v, _| {
                assert_eq!(v.curation_state.collection_history.redo.len(), MAX_RECORDS);
            });
        }
        if mode == "full_redo" {
            let revision = browser.update(cx, |v, _| v.curation_state.lineages[&key].revision);
            let external_request = browser.update(cx, |v, _| v.curation_state.next_request_id());
            cx.update_global::<SourceRegistryGlobal, _>(|g, _| {
                g.submit_catalog(CatalogCommand::ApplyCurationOperation {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                    fence: SourceCurationOperationFence::ExactRevision(revision),
                    root_identity: snapshot.root_identity.clone(),
                    request_id: external_request,
                    batches: vec![vec![SourceCurationUpdate::SetLastPlayedAt {
                        path: RootRelativePath::try_from_path(paths[0].clone()).unwrap(),
                        played_at: Some(8),
                    }]],
                })
            })
            .unwrap();
            let changed = browser_owner_event(&browser, cx);
            let CatalogEvent::CurationOperationCompleted { outcome, .. } = changed else {
                unreachable!()
            };
            assert!(outcome.is_success()); // genuine unobserved concurrent revision, forces refusal
            browser.update(cx, |v, _| {
                v.curation_state.next_request_id = external_request
            });
        }
        if mode == "pending" {
            browser.update(cx, |v, _| {
                let CurationPlan::Ready(plan) = v.curation_state.plan_action(
                    &key,
                    CurationAction::RecordPlayback { played_at: 99 },
                    &paths,
                ) else {
                    panic!()
                };
                assert!(v.curation_state.stage_latest(&key, &plan));
                let CurationPlan::Ready(unlock) =
                    v.curation_state
                        .plan_action(&key, CurationAction::UnlockKeepRating, &paths)
                else {
                    panic!()
                };
                assert!(
                    v.capture_collection_history_presentation(&key, &unlock)
                        .is_none()
                );
                v.curation_state
                    .lineages
                    .get_mut(&key)
                    .unwrap()
                    .staged_latest
                    .take();
            });
        }
        if mode == "rejected" || mode == "full_redo_rejected" {
            cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
            browser.update(cx, |v, cx| {
                v.submit_curation_action(CurationAction::UnlockKeepRating, Some(paths.clone()), cx);
                assert!(v.curation_state.collection_history.pending.is_none());
                assert!(v.curation_state.collection_history.undo.is_empty());
                if mode == "full_redo_rejected" {
                    assert_eq!(v.curation_state.collection_history.redo.len(), MAX_RECORDS);
                }
            });
            return;
        }
        browser.update(cx, |v, cx| {
            if mode == "full_redo" {
                let CurationPlan::Ready(plan) =
                    v.curation_state
                        .plan_action(&key, CurationAction::UnlockKeepRating, &paths)
                else {
                    panic!()
                };
                // Actual stale owner refusal, not a fabricated failed receipt.
                v.submit_planned_curation(&key, plan, false, cx).unwrap();
            } else {
                let CurationPlan::Ready(plan) =
                    v.curation_state
                        .plan_action(&key, CurationAction::UnlockKeepRating, &paths)
                else {
                    panic!(
                        "unlock plan missing: {:?}",
                        v.curation_state.status_for(&key)
                    )
                };
                assert!(
                    v.capture_collection_history_presentation(&key, &plan)
                        .is_some(),
                    "presentation excluded"
                );
                v.submit_curation_action(CurationAction::UnlockKeepRating, Some(paths.clone()), cx);
            }
            v.source_registry_task.take();
            assert!(v.curation_state.collection_history.undo.is_empty());
            assert_eq!(
                v.curation_state.collection_history.pending.is_some(),
                count <= 128 && mode != "full_redo",
                "status={:?} pending={:?}",
                v.curation_state.status_for(&key),
                v.curation_state.lineages[&key].pending
            );
        });
        let mut event = browser_owner_event(&browser, cx);
        if mode == "stale_publication" {
            if let CatalogEvent::CurationOperationCompleted { publication, .. } = &mut event {
                *publication = CatalogPublicationStatus::NonProjectable;
            }
        }
        if mode == "full_redo" {
            let CatalogEvent::CurationOperationCompleted { outcome, .. } = &event else {
                unreachable!()
            };
            assert!(!outcome.is_success());
            assert!(outcome.committed_batches().is_empty());
            browser.update(cx, |v, cx| {
                v.apply_curation_event(&event, cx);
                assert_eq!(v.curation_state.collection_history.redo.len(), MAX_RECORDS);
                assert!(v.curation_state.collection_history.undo.is_empty());
            });
            cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
            return;
        }
        if mode == "wrong_rating" || mode == "wrong_lock" || mode == "missing_path" {
            // Fault injection at the consumer DTO boundary, derived from a genuine committed event.
            // This cannot manufacture a SourceWriter receipt or qualify production mutation.
            let CatalogEvent::CurationOperationCompleted {
                outcome,
                request_id,
                ..
            } = &event
            else {
                unreachable!()
            };
            let mut decoded = CurationOutcome::from_catalog(outcome);
            if mode == "missing_path" {
                decoded.committed_batches[0].files.clear();
            } else {
                let row = decoded.committed_batches[0]
                    .files
                    .get_mut(&paths[0])
                    .unwrap();
                *row = AudioFileCuration::from_parts(
                    if mode == "wrong_rating" { 2 } else { 3 },
                    mode == "wrong_lock",
                    row.collection_mask(),
                    row.last_played_at(),
                    row.last_curated_at(),
                )
                .unwrap();
            }
            browser.update(cx, |v, _| {
                v.curation_state
                    .apply_outcome(key.clone(), *request_id, decoded);
                assert!(v.curation_state.collection_history.undo.is_empty());
                assert!(v.curation_state.lineages[&key].history_unresolved);
            });
        } else if mode == "stale_publication" {
            browser.update(cx, |v, cx| {
                v.apply_curation_event(&event, cx);
                assert!(v.curation_state.collection_history.undo.is_empty());
                assert!(!v.collection_history_ready(Direction::Undo));
            });
        } else if mode == "lost" {
            // The actual commit exists; retiring a pending consumer must retain uncertainty.
            browser.update(cx, |v, _| {
                v.curation_state.prune_completed_except(None);
                assert!(v.curation_state.lineages[&key].history_unresolved);
                assert!(v.curation_state.collection_history.undo.is_empty());
            });
        } else if mode == "stale" {
            browser.update(cx, |v, cx| {
                v.catalog_activation.as_mut().unwrap().lifecycle_generation = 2;
                v.apply_curation_event(&event, cx);
                assert!(v.curation_state.collection_history.undo.is_empty());
                assert!(!v.collection_history_ready(Direction::Undo));
            });
        } else {
            browser.update(cx, |v, cx| {
                assert!(v.apply_curation_event(&event, cx));
                assert_eq!(
                    v.curation_state.collection_history.undo.len(),
                    usize::from(count <= 128)
                );
                let sizes = (
                    v.curation_state.collection_history.undo.len(),
                    v.curation_state.collection_history.redo.len(),
                );
                v.apply_curation_event(&event, cx);
                assert_eq!(
                    (
                        v.curation_state.collection_history.undo.len(),
                        v.curation_state.collection_history.redo.len()
                    ),
                    sizes
                );
                v.submit_curation_action(CurationAction::UnlockKeepRating, Some(paths.clone()), cx);
                assert!(v.curation_state.collection_history.pending.is_none()); // genuine no-op
                assert!(v.curation_state.lineages[&key].pending.is_empty());
            });
            if count <= 128 {
                if mode == "mixed" {
                    browser.update(cx, |v, cx| {
                        v.submit_curation_action(
                            CurationAction::ToggleCollection { slot: 0 },
                            Some(paths.clone()),
                            cx,
                        );
                        v.source_registry_task.take();
                    });
                    apply(&browser, cx);
                    browser.update(cx, |v, _| {
                        assert_eq!(v.curation_state.collection_history.undo.len(), 2)
                    });
                    click(&browser, &fixture, cx, Direction::Undo);
                    apply(&browser, cx);
                    browser.update(cx, |v, _| {
                        assert!(!v.entries[0].curation.unwrap().rating().locked())
                    });
                }
                for (direction, locked) in [(Direction::Undo, true), (Direction::Redo, false)] {
                    click(&browser, &fixture, cx, direction);
                    let event = apply(&browser, cx);
                    let CatalogEvent::CurationOperationCompleted { outcome, .. } = &event else {
                        unreachable!()
                    };
                    assert!(outcome.is_success());
                    assert_eq!(outcome.committed_batches().len(), 1);
                    cx.update(|_, app| {
                        app.global::<SourceRegistryGlobal>().submit_catalog(
                            CatalogCommand::ReadSnapshot {
                                source_id: source_id.clone(),
                                expected_revision: outcome.final_revision(),
                            },
                        )
                    })
                    .unwrap();
                    let CatalogEvent::SnapshotReady { snapshot: actual } = next_event(
                        |events| {
                            cx.update(|_, app| {
                                app.global::<SourceRegistryGlobal>()
                                    .drain_catalog_events(events)
                            });
                        },
                        |e| matches!(e, CatalogEvent::SnapshotReady { .. }),
                    ) else {
                        unreachable!()
                    };
                    browser.update(cx, |v, _| {
                        for e in &v.entries {
                            let c = e.curation.unwrap();
                            let sql =
                                curation_from_metadata(&actual.curation_metadata[&e.relative_path])
                                    .unwrap();
                            assert_eq!(c, sql);
                            assert_eq!(c.rating().value(), 3);
                            assert_eq!(c.rating().locked(), locked);
                            assert!(c.has_collection(1));
                            assert_eq!(c.last_played_at(), original.last_played_at());
                            assert!(c.last_curated_at().is_some());
                        }
                        assert_eq!(v.selection.anchor(), Some(&original_id));
                        assert_eq!(v.browser_focus_id(), Some(original_id.clone()));
                    });
                }
                click(&browser, &fixture, cx, Direction::Undo);
                apply(&browser, cx);
                browser.update(cx, |v, cx| {
                    v.submit_curation_action(
                        CurationAction::ToggleCollection { slot: 0 },
                        Some(paths.clone()),
                        cx,
                    );
                    v.source_registry_task.take();
                });
                apply(&browser, cx);
                browser.update(cx, |v, _| {
                    assert!(v.curation_state.collection_history.redo.is_empty())
                });
            }
        }
        // Read persisted state independently of the UI, including lost/stale delivery cases.
        cx.update(|_, app| {
            app.global::<SourceRegistryGlobal>()
                .submit_catalog(CatalogCommand::ReadSnapshot {
                    source_id: source_id.clone(),
                    expected_revision: None,
                })
        })
        .unwrap();
        let CatalogEvent::SnapshotReady { snapshot: actual } = next_event(
            |events| {
                cx.update(|_, app| {
                    app.global::<SourceRegistryGlobal>()
                        .drain_catalog_events(events)
                });
            },
            |e| matches!(e, CatalogEvent::SnapshotReady { .. }),
        ) else {
            unreachable!()
        };
        assert_eq!(actual.curation_metadata.len(), count);
        for m in actual.curation_metadata.values() {
            let row = curation_from_metadata(m).unwrap();
            assert_eq!(row.rating().value(), 3);
            assert!(row.has_collection(1));
            assert_eq!(row.last_played_at(), Some(7));
        }
        cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
    }
    #[gpui::test]
    fn sample_history_real_gpui_apply_undo_redo(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "normal");
    }
    #[gpui::test]
    fn sample_history_real_gpui_mixed_chronology(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "mixed");
    }
    #[gpui::test]
    fn sample_history_real_gpui_atomic_128(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 128, "normal");
    }
    #[gpui::test]
    fn sample_history_real_gpui_ordinary_129_without_history(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 129, "normal");
    }
    #[gpui::test]
    fn sample_history_real_gpui_pending_latest_excluded(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "pending");
    }
    #[gpui::test]
    fn sample_history_real_gpui_lost_actual_delivery(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "lost");
    }
    #[gpui::test]
    fn sample_history_real_gpui_stale_source(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "stale");
    }
    #[gpui::test]
    fn sample_history_real_gpui_owner_rejection(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "rejected");
    }
    #[gpui::test]
    fn sample_history_real_gpui_full_redo_failed_original_preserved(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "full_redo");
    }

    #[gpui::test]
    fn sample_history_real_gpui_wrong_rating(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "wrong_rating");
    }

    #[gpui::test]
    fn sample_history_real_gpui_wrong_lock(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "wrong_lock");
    }

    #[gpui::test]
    fn sample_history_real_gpui_missing_path(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "missing_path");
    }

    #[gpui::test]
    fn sample_history_real_gpui_stale_publication(cx: &mut gpui::TestAppContext) {
        sample_history_actual(cx, 1, "stale_publication");
    }

    #[gpui::test]
    fn sample_history_real_gpui_full_redo_rejected_original_preserved(
        cx: &mut gpui::TestAppContext,
    ) {
        sample_history_actual(cx, 1, "full_redo_rejected");
    }
}
