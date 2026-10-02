#[cfg(test)]
mod harness {
    //! Model/GPUI fixtures and actual read-only catalog integration; no native qualification.
    use super::super::*;
    use gpui::{Keystroke, TestAppContext};
    use yield_session::{
        CatalogIndexPageError, RetainedIndexCompleteness, SourceIndexInspectionError,
    };
    fn empty_page(model: &Model, available: bool, revision: u64) -> CatalogEvent {
        let b = model.binding.as_ref().unwrap();
        CatalogEvent::IndexPageReady {
            source_id: b.source_id.clone(),
            lifecycle_generation: b.generation,
            query_id: b.query_id,
            completeness: RetainedIndexCompleteness::RetainedUnknown,
            page: CatalogIndexPage {
                schema_available: available,
                index_revision: revision,
                entries: Vec::new(),
                next: None,
                inspection_handles: Vec::new(),
            },
        }
    }
    #[test]
    fn retained_modal_cancels_and_rejects_closed_restarted_and_foreign_pages() {
        let mut model = Model::default();
        let id = SourceId::from_string("source");
        model.begin(id.clone(), 1);
        assert!(matches!(
            model.page_command(),
            Some(CatalogCommand::RequestIndexPage {
                page_limit: 128,
                after_cursor: None,
                ..
            })
        ));
        let mut old = empty_page(&model, true, 2);
        let old_binding = model.cancel().unwrap();
        assert!(!model.apply(&mut old));
        assert!(model.page.is_none());
        model.begin(id, 2);
        assert_ne!(
            model.binding.as_ref().unwrap().query_id,
            old_binding.query_id
        );
        assert!(!model.apply(&mut old));
        let mut current = empty_page(&model, true, 3);
        assert!(model.apply(&mut current));
        assert!(model.status.contains("No retained entries"));
        assert!(model.status.contains("unknown"));
        assert!(!model.apply(&mut current));
        assert!(!model.change_page(true));
        assert!(!model.change_page(false));
        assert!(!model.prepare_inspection(usize::MAX));
        assert!(model.inspection_command().is_none());
    }
    #[test]
    fn retained_modal_exposes_legacy_stale_and_malformed_states_without_empty_success() {
        let mut model = Model::default();
        let id = SourceId::from_string("source");
        model.begin(id.clone(), 1);
        let mut legacy = empty_page(&model, false, 0);
        assert!(model.apply(&mut legacy));
        assert!(model.status.contains("unavailable"));
        model.begin(id.clone(), 1);
        let b = model.binding.clone().unwrap();
        let mut stale = CatalogEvent::IndexPageFailed {
            source_id: b.source_id,
            lifecycle_generation: b.generation,
            query_id: b.query_id,
            error: CatalogIndexPageError::StaleRevision {
                expected: 1,
                actual: 2,
            },
        };
        assert!(model.apply(&mut stale));
        assert!(model.page.is_none());
        assert!(model.status.contains("changed"));
        model.begin(id, 1);
        model.pending.as_mut().unwrap().expected = Some(5);
        let mut wrong_revision = empty_page(&model, true, 6);
        assert!(model.apply(&mut wrong_revision));
        assert!(model.page.is_none());
        let mut malformed = CatalogIndexPage {
            schema_available: true,
            index_revision: 1,
            entries: Vec::new(),
            next: None,
            inspection_handles: Vec::new(),
        };
        let entry = SourceIndexEntry {
            relative_path: "a.txt".into(),
            classification: SourceIndexClassification::Inaccessible,
            file_size: None,
            modified_ns: None,
            file_identity: None,
            diagnostic: None,
            format_policy_version: 1,
        };
        malformed.entries = vec![entry.clone(); 129];
        assert!(!bounded_page(&malformed));
        malformed.entries = vec![entry];
        assert!(!bounded_page(&malformed)); // No owner handles: public facts alone cannot be accepted.
        assert!(!eligible(&malformed.entries[0]));
    }
    #[test]
    fn retained_modal_late_and_canceled_inspection_outcomes_never_authorize_reveal() {
        let mut model = Model::default();
        model.begin(SourceId::from_string("source"), 1);
        let cancellation = IndexInspectionCancellation::default();
        // Completion-state fixture only; it mints no page handle or successful production inspection.
        model.inspection_pending = Some(PendingInspection {
            row: 0,
            request_id: 3,
            cancellation: cancellation.clone(),
            admitted: true,
        });
        let b = model.binding.clone().unwrap();
        let mut event = CatalogEvent::IndexInspectionCompleted {
            source_id: b.source_id,
            lifecycle_generation: b.generation,
            query_id: b.query_id,
            request_id: 2,
            cancellation: cancellation.clone(),
            result: Err(SourceIndexInspectionError::Unsupported),
        };
        assert!(!model.apply(&mut event));
        assert!(model.inspection_pending.is_some());
        if let CatalogEvent::IndexInspectionCompleted { request_id, .. } = &mut event {
            *request_id = 3;
        }
        assert!(model.apply(&mut event));
        assert!(model.inspection.is_none());
        assert!(model.status.contains("unavailable on this platform"));
        model.inspection_pending = Some(PendingInspection {
            row: 0,
            request_id: 3,
            cancellation: cancellation.clone(),
            admitted: true,
        });
        model.cancel();
        assert!(cancellation.is_canceled());
        assert!(!model.apply(&mut event));
    }
    #[test]
    fn retained_modal_display_preserves_unicode_and_labels_raw_bytes_with_fixed_bounds() {
        assert_eq!(
            display_path(Path::new("échantillons/日本語.txt")),
            "échantillons/日本語.txt"
        );
        let huge = "界".repeat(1000);
        let label = display_path(Path::new(&huge));
        assert!(label.len() <= DISPLAY_BYTES + 32);
        assert!(label.ends_with("display clipped)"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec());
            assert_eq!(
                display_path(Path::new(&raw)),
                "Raw path bytes: 72 61 77 2d ff 2e 74 78 74"
            );
            let raw = std::ffi::OsString::from_vec(vec![0xff; 2048]);
            assert!(display_path(Path::new(&raw)).len() < DISPLAY_BYTES);
        }
        assert_eq!(
            [MODAL_ID, LIST_ID, STATUS_ID, CLOSE_ID],
            [
                "unsupported-entries-modal",
                "unsupported-entries-list",
                "unsupported-entries-status",
                "unsupported-entries-close"
            ]
        );
    }
    #[gpui::test]
    fn retained_modal_escape_consumes_background_keys_and_restores_focus(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let search = view.search_input.read(cx).focus_handle();
                window.focus(&search);
                view.open_unsupported_files(window, cx);
                assert!(view.unsupported_entries.is_open());
                assert!(view.unsupported_entries.focus.is_focused(window));
                assert!(
                    view.unsupported_entries
                        .model
                        .status
                        .contains("No registered source")
                );
                let space = KeyDownEvent {
                    keystroke: Keystroke::parse("space").unwrap(),
                    is_held: false,
                };
                assert!(view.handle_unsupported_key(&space, window, cx));
                assert!(view.unsupported_entries.is_open());
                let escape = KeyDownEvent {
                    keystroke: Keystroke::parse("escape").unwrap(),
                    is_held: false,
                };
                assert!(view.handle_unsupported_key(&escape, window, cx));
                assert!(!view.unsupported_entries.is_open());
                assert!(search.is_focused(window));
                assert!(!view.handle_unsupported_key(&space, window, cx));
            });
        });
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn retained_modal_uses_real_owner_pages_bounded_history_and_held_inspection() {
        use std::{
            fs,
            time::{Duration, Instant},
        };
        use yield_library::app_dirs::{
            ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard,
        };
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("ui-retained-inspection");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        for index in 0..SOURCE_INDEX_PAGE_MAX_ENTRIES * (HISTORY_LIMIT + 2) {
            fs::write(
                root.path().join(format!("entry-{index:04}.txt")),
                b"retained facts",
            )
            .unwrap();
        }
        // Actual production read-only catalog/lease route. This does not provide journal mutation capacity or native qualification.
        let service = yield_catalog::CatalogService::start(&guard).unwrap();
        let id = SourceId::from_string("ui-retained-owner");
        let receive = |predicate: &dyn Fn(&CatalogEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Ok(event) = service.try_recv_event() {
                    if predicate(&event) {
                        break event;
                    }
                }
                assert!(Instant::now() < deadline, "owner event timeout");
                std::thread::yield_now();
            }
        };
        service
            .submit(CatalogCommand::Register {
                source_id: id.clone(),
                canonical_root: root.path().into(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive(&|e| matches!(e, CatalogEvent::Registered { .. }));
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: id.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        receive(&|e| matches!(e, CatalogEvent::AuditCommitted { .. }));
        let mut model = Model::default();
        model.begin(id, 1);
        for page_number in 0..=HISTORY_LIMIT + 1 {
            service.submit(model.page_command().unwrap()).unwrap();
            model.pending.as_mut().unwrap().admitted = true;
            let mut event = receive(&|e| {
                matches!(
                    e,
                    CatalogEvent::IndexPageReady { .. } | CatalogEvent::IndexPageFailed { .. }
                )
            });
            assert!(model.apply(&mut event));
            assert_eq!(model.page.as_ref().unwrap().entries.len(), 128);
            assert!(model.history.len() <= HISTORY_LIMIT);
            if page_number < HISTORY_LIMIT + 1 {
                assert!(model.change_page(true));
                assert!(model.page.is_none());
            }
        }
        assert_eq!(model.history.len(), HISTORY_LIMIT);
        assert!(model.page.as_ref().unwrap().next.is_none());
        assert!(model.prepare_inspection(0));
        service.submit(model.inspection_command().unwrap()).unwrap();
        model.inspection_pending.as_mut().unwrap().admitted = true;
        let mut event = receive(&|e| matches!(e, CatalogEvent::IndexInspectionCompleted { .. }));
        assert!(model.apply(&mut event));
        assert!(model.inspection.as_ref().unwrap().is_live());
        assert_eq!(
            model.status,
            "Entry inspected by source owner. Native reveal unavailable."
        );
        assert!(model.select_row(1));
        assert!(model.inspection.is_none());
        assert!(model.prepare_inspection(1));
        let cancellation = model
            .inspection_pending
            .as_ref()
            .unwrap()
            .cancellation
            .clone();
        assert!(model.select_row(0));
        assert!(cancellation.is_canceled());
        assert!(model.inspection_pending.is_none());
        let old_epoch = model.page_epoch;
        assert!(model.change_page(false));
        assert!(model.inspection.is_none());
        service.submit(model.page_command().unwrap()).unwrap();
        let mut event = receive(&|e| matches!(e, CatalogEvent::IndexPageReady { .. }));
        assert!(model.apply(&mut event));
        assert_eq!(model.history.len(), HISTORY_LIMIT - 1);
        assert!(!model.select_row_for_page(0, old_epoch));
        assert!(model.select_row_for_page(0, model.page_epoch));
        let old = model.cancel().unwrap();
        service.cancel_index_query(&old.source_id, old.generation, old.query_id);
        assert!(!model.apply(&mut event));
        assert!(model.page.is_none());
    }
}
