//! Read-only, source-scoped projection for the status-bar Job Details surface.

use std::path::PathBuf;

use super::Browser;

/// A currently supported kind of background work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JobKind {
    SourceScan,
    Catalog,
    Waveform,
    AudioOutput,
}

impl JobKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::SourceScan => "Source scan",
            Self::Catalog => "Source catalog",
            Self::Waveform => "Waveform",
            Self::AudioOutput => "Audio output",
        }
    }
}

/// A bounded presentation row. These workers do not currently publish known
/// semantic totals, so running rows are explicitly indeterminate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JobRow {
    pub(super) kind: JobKind,
    pub(super) source: Option<PathBuf>,
    pub(super) generation: u64,
    pub(super) work: &'static str,
    pub(super) blocked: Option<String>,
}

impl JobRow {
    pub(super) fn state_label(&self) -> &'static str {
        if self.blocked.is_some() {
            "Needs attention"
        } else {
            "In progress · time remaining unknown"
        }
    }

    pub(super) fn source_label(&self) -> String {
        let label = self
            .source
            .as_ref()
            .and_then(|root| root.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Current session".into());
        bounded_text(&label, 32)
    }

    pub(super) fn blocked_label(&self) -> Option<String> {
        self.blocked.as_ref().map(|error| bounded_text(error, 100))
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut text: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

/// The current projection replaces older rows on every render; no old source
/// or job generation is retained by the popover.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct JobSnapshot {
    pub(super) rows: Vec<JobRow>,
}

impl JobSnapshot {
    pub(super) fn active_count(&self) -> usize {
        self.rows.iter().filter(|row| row.blocked.is_none()).count()
    }

    pub(super) fn status_label(&self) -> String {
        let count = self.active_count();
        if count > 0 {
            format!("{count} job{} running", if count == 1 { "" } else { "s" })
        } else if self.rows.iter().any(|row| row.blocked.is_some()) {
            "Job needs attention".into()
        } else {
            "Jobs idle".into()
        }
    }
}

pub(super) fn snapshot(browser: &Browser) -> JobSnapshot {
    let mut rows = Vec::with_capacity(4);
    let active_root = browser.root.as_ref();
    if browser.scan_cancel.is_some()
        && let Some(root) = active_root
    {
        rows.push(JobRow {
            kind: JobKind::SourceScan,
            source: Some(root.clone()),
            generation: browser.scan_generation,
            work: "Finding audio files",
            blocked: None,
        });
    }
    if let Some(activation) = browser.catalog_activation.as_ref()
        && active_root == Some(&activation.root)
    {
        if !activation.audit_finished || browser.catalog_projection_task.is_some() {
            rows.push(JobRow {
                kind: JobKind::Catalog,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Checking source catalog",
                blocked: None,
            });
        } else if let Some(error) = activation.audit_error.as_ref() {
            rows.push(JobRow {
                kind: JobKind::Catalog,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Refresh source to retry",
                blocked: Some(error.clone()),
            });
        }
    }
    if browser.waveform_pending || browser.waveform_detail.is_pending() {
        rows.push(JobRow {
            kind: JobKind::Waveform,
            source: active_root.cloned(),
            generation: browser.waveform_generation,
            work: "Loading selected waveform",
            blocked: None,
        });
    }
    if browser.audio_output_open_task.is_some() && browser.playback_init_intent.is_some() {
        rows.push(JobRow {
            kind: JobKind::AudioOutput,
            source: active_root.cloned(),
            generation: browser.playback_init_generation,
            work: "Starting audio output",
            blocked: None,
        });
    }
    JobSnapshot { rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use gpui::TestAppContext;
    use yield_library::sample_sources::SourceId;

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
        let jobs = browser.update(cx, |view, _| snapshot(view));
        assert_eq!(jobs.status_label(), "2 jobs running");
        assert_eq!(jobs.rows[0].generation, 7);
        assert_eq!(jobs.rows[1].generation, 11);
        assert!(jobs.rows.iter().all(|row| row.source_label() == "first"));

        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/sources/second"));
            view.scan_generation = 8;
            view.catalog_activation = Some(catalog("/sources/first", 11, None));
        });
        let jobs = browser.update(cx, |view, _| snapshot(view));
        assert_eq!(jobs.status_label(), "1 job running");
        assert_eq!(jobs.rows[0].generation, 8);
        assert_eq!(jobs.rows[0].source_label(), "second");
        assert!(jobs.rows.iter().all(|row| row.kind != JobKind::Catalog));

        browser.update(cx, |view, _| {
            view.scan_cancel = None;
            view.catalog_activation = Some(catalog("/sources/second", 12, Some("Retry needed")));
        });
        let jobs = browser.update(cx, |view, _| snapshot(view));
        assert_eq!(jobs.status_label(), "Job needs attention");
        assert_eq!(jobs.rows[0].generation, 12);
        assert_eq!(
            jobs.rows[0].blocked_label().as_deref(),
            Some("Retry needed")
        );

        browser.update(cx, |view, _| view.catalog_activation = None);
        assert_eq!(
            browser.update(cx, |view, _| snapshot(view)).status_label(),
            "Jobs idle"
        );
    }

    #[test]
    fn status_counts_only_live_work_and_keeps_blocked_distinct() {
        let mut snapshot = JobSnapshot::default();
        assert_eq!(snapshot.status_label(), "Jobs idle");
        snapshot.rows.push(JobRow {
            kind: JobKind::Catalog,
            source: Some(PathBuf::from("/source")),
            generation: 4,
            work: "Refresh source to retry",
            blocked: Some("Source unavailable".into()),
        });
        assert_eq!(snapshot.status_label(), "Job needs attention");
        snapshot.rows.push(JobRow {
            kind: JobKind::Waveform,
            source: Some(PathBuf::from("/source")),
            generation: 8,
            work: "Loading selected waveform",
            blocked: None,
        });
        assert_eq!(snapshot.status_label(), "1 job running");
        assert_eq!(
            snapshot.rows[1].state_label(),
            "In progress · time remaining unknown"
        );
        assert_eq!(snapshot.rows[0].source_label(), "source");
        assert_eq!(bounded_text("abcdef", 3), "abc…");
    }
}
