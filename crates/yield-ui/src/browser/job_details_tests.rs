#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use gpui::TestAppContext;
    use yield_library::sample_sources::SourceId;

    use super::super::SourceSession;
    use super::super::catalog::CatalogActivation;
    use crate::theme::ActiveTheme;

    fn catalog(root: &str, generation: u64, error: Option<&str>) -> CatalogActivation {
        CatalogActivation {
            source_id: SourceId::from_string(root),
            root: PathBuf::from(root),
            lifecycle_generation: generation,
            registered: true,
            registration_admitted: true,
            root_identity: None,
            audit_finished: error.is_some(),
            audit_error: error.map(str::to_owned),
            audit_retryable: error.is_some(),
            force_reload: false,
            fallback_to_scan: false,
        }
    }

    #[gpui::test]
    fn projection_tracks_current_source_and_running_generation(cx: &mut TestAppContext) {
        cx.set_global(ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/sources/first"));
            view.scan_generation = 7;
            view.scan_cancel = Some(Arc::new(AtomicBool::new(false)));
            view.catalog_activation = Some(catalog("/sources/first", 11, None));
        });
        let jobs = browser.update(cx, |view, cx| snapshot(view, cx));
        assert_eq!(jobs.status_label(), "2 jobs active");
        assert_eq!(jobs.rows[0].generation, 7);
        assert_eq!(jobs.rows[1].generation, 11);
        assert!(jobs.rows.iter().all(|row| row.source_label() == "first"));

        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/sources/second"));
            view.scan_generation = 8;
            view.catalog_activation = Some(catalog("/sources/first", 11, None));
        });
        let jobs = browser.update(cx, |view, cx| snapshot(view, cx));
        assert_eq!(jobs.status_label(), "1 job active");
        assert_eq!(jobs.rows[0].generation, 8);
        assert_eq!(jobs.rows[0].source_label(), "second");
        assert!(jobs.rows.iter().all(|row| row.kind != JobKind::Catalog));

        browser.update(cx, |view, _| {
            view.scan_cancel = None;
            view.catalog_activation = Some(catalog("/sources/second", 12, Some("Retry needed")));
        });
        let jobs = browser.update(cx, |view, cx| snapshot(view, cx));
        assert_eq!(jobs.status_label(), "Job needs attention");
        assert_eq!(jobs.rows[0].generation, 12);
        assert!(jobs.rows[0].can_retry);
        assert_eq!(
            jobs.rows[0].blocked_label().as_deref(),
            Some("Retry needed")
        );

        browser.update(cx, |view, _| {
            view.catalog_activation.as_mut().unwrap().audit_retryable = false;
        });
        let jobs = browser.update(cx, |view, cx| snapshot(view, cx));
        assert!(!jobs.rows[0].can_retry);

        browser.update(cx, |view, _| view.catalog_activation = None);
        assert_eq!(
            browser
                .update(cx, |view, cx| snapshot(view, cx))
                .status_label(),
            "Jobs idle"
        );
    }

    #[gpui::test]
    fn rejected_actions_retain_recoverable_source_and_stale_actions_disappear(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            let root = PathBuf::from("/sources/first");
            let mut session = SourceSession::new(root.clone());
            session.source_id = Some(SourceId::from_string("/sources/first"));
            view.source_sessions.push(session);
            view.active_source = Some(0);
            view.root = Some(root);
            view.catalog_activation = Some(catalog("/sources/first", 11, Some("Retry needed")));
        });
        browser.update(cx, |view, cx| {
            assert!(snapshot(view, cx).rows[0].can_retry);
            view.retry_job_audit(cx);
            assert_eq!(view.job_action, None);
            assert!(
                view.job_action_error
                    .as_deref()
                    .is_some_and(|error| error.contains("not accepted"))
            );
            assert!(snapshot(view, cx).rows[0].can_retry);
            view.catalog_activation.as_mut().unwrap().audit_finished = false;
            view.job_action_error = None;
            assert!(!snapshot(view, cx).rows[0].can_cancel);
            view.cancel_job_audit(cx);
            assert_eq!(view.job_action, None);
            assert!(
                view.job_action_error
                    .as_deref()
                    .is_some_and(|error| error.contains("no longer cancelable"))
            );
            assert!(!snapshot(view, cx).rows[0].can_cancel);
            view.job_action = Some(JobActionState {
                source_id: SourceId::from_string("/sources/first"),
                lifecycle_generation: 11,
                kind: JobActionKind::CancelRequested,
            });
            assert!(!snapshot(view, cx).rows[0].can_cancel);
            view.root = Some(PathBuf::from("/sources/second"));
            assert!(snapshot(view, cx).rows.is_empty());
        });
    }

    #[test]
    fn status_counts_only_live_work_and_keeps_blocked_distinct() {
        let mut snapshot = JobSnapshot::default();
        assert_eq!(snapshot.status_label(), "Jobs idle");
        snapshot.rows.push(JobRow {
            kind: JobKind::Catalog,
                feature_state: None,
            source: Some(PathBuf::from("/source")),
            generation: 4,
            work: "Retry this source audit".into(),
            completed: false,
            blocked: Some("Source unavailable".into()),
            action: None,
            can_retry: true,
            can_cancel: false,
        });
        assert_eq!(snapshot.status_label(), "Job needs attention");
        snapshot.rows.push(JobRow {
            kind: JobKind::Waveform,
                feature_state: None,
            source: Some(PathBuf::from("/source")),
            generation: 8,
            work: "Loading selected waveform".into(),
            completed: false,
            blocked: None,
            action: None,
            can_retry: false,
            can_cancel: false,
        });
        assert_eq!(snapshot.status_label(), "1 job active");
        assert_eq!(
            snapshot.rows[1].state_label(),
            "In progress · time remaining unknown"
        );
        assert_eq!(snapshot.rows[0].source_label(), "source");
        assert_eq!(bounded_text("abcdef", 3), "abc…");
    }
    #[test]
    fn selected_file_completion_is_idle_and_mixed_work_has_no_overall_percentage() {
        let mut snapshot = JobSnapshot {
            rows: vec![JobRow {
                kind: JobKind::FeatureAnalysis,
                feature_state: None,
                source: Some("/source".into()),
                generation: 2,
                work: "Features ready: 1 / 1 readiness targets".into(),
                completed: true,
                blocked: None,
                action: None,
                can_retry: false,
                can_cancel: false,
            }],
        };
        assert_eq!(snapshot.active_count(), 0);
        assert_eq!(snapshot.status_label(), "Jobs idle");
        assert_eq!(snapshot.rows[0].state_label(), "Selected file ready");
        snapshot.rows[0].completed = false;
        assert!(snapshot.status_label().starts_with("source · "));
        let mut scan = snapshot.rows[0].clone();
        scan.kind = JobKind::SourceScan;
        scan.source = Some("/other-source".into());
        scan.work = "Finding audio files".into();
        snapshot.rows.push(scan);
        assert_eq!(snapshot.status_label(), "2 jobs active");
        snapshot.rows[0].blocked = Some("Analysis failed".into());
        assert_eq!(snapshot.status_label(), "1 job active");
    }
}
