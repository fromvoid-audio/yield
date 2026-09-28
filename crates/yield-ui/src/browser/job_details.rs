//! Read-only, source-scoped projection for the status-bar Job Details surface.

use gpui::{BorrowAppContext, Context};
use std::path::PathBuf;
use yield_library::sample_sources::SourceId;
use yield_session::CatalogCommand;

use super::Browser;
use super::source_registry::SourceRegistryGlobal;

/// A currently supported kind of background work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JobKind {
    SourceScan,
    Catalog,
    Waveform,
    AudioOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JobActionKind {
    RetryQueued,
    CancelRequested,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JobActionState {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
    pub(super) kind: JobActionKind,
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
    pub(super) action: Option<JobActionKind>,
    pub(super) can_retry: bool,
    pub(super) can_cancel: bool,
}

impl JobRow {
    pub(super) fn state_label(&self) -> &'static str {
        if matches!(self.action, Some(JobActionKind::RetryQueued)) {
            "Retry queued · awaiting audit"
        } else if matches!(self.action, Some(JobActionKind::CancelRequested)) {
            "Cancellation requested · awaiting worker"
        } else if self.blocked.is_some() {
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

pub(super) fn snapshot(browser: &Browser, cx: &Context<Browser>) -> JobSnapshot {
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
            action: None,
            can_retry: false,
            can_cancel: false,
        });
    }
    if let Some(activation) = browser.catalog_activation.as_ref()
        && active_root == Some(&activation.root)
    {
        let audit_running = cx.has_global::<SourceRegistryGlobal>()
            && cx
                .global::<SourceRegistryGlobal>()
                .catalog_audit_is_running(&activation.source_id, activation.lifecycle_generation);
        let action = browser
            .job_action
            .as_ref()
            .filter(|pending| {
                pending.source_id == activation.source_id
                    && pending.lifecycle_generation == activation.lifecycle_generation
            })
            .map(|pending| pending.kind);
        if !activation.audit_finished || browser.catalog_projection_task.is_some() {
            rows.push(JobRow {
                kind: JobKind::Catalog,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Checking source catalog",
                blocked: None,
                action,
                can_retry: false,
                can_cancel: activation.registered
                    && action.is_none()
                    && !activation.audit_finished
                    && audit_running,
            });
        } else if let Some(error) = activation.audit_error.as_ref() {
            rows.push(JobRow {
                kind: JobKind::Catalog,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Retry this source audit",
                blocked: Some(error.clone()),
                action,
                can_retry: action.is_none() && activation.registered && activation.audit_retryable,
                can_cancel: false,
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
            action: None,
            can_retry: false,
            can_cancel: false,
        });
    }
    if browser.audio_output_open_task.is_some() && browser.playback_init_intent.is_some() {
        rows.push(JobRow {
            kind: JobKind::AudioOutput,
            source: active_root.cloned(),
            generation: browser.playback_init_generation,
            work: "Starting audio output",
            blocked: None,
            action: None,
            can_retry: false,
            can_cancel: false,
        });
    }
    JobSnapshot { rows }
}

impl Browser {
    fn current_job_audit(&self) -> Option<(SourceId, u64)> {
        let activation = self.catalog_activation.as_ref()?;
        let session = self
            .active_source
            .and_then(|index| self.source_sessions.get(index))?;
        if self.root.as_ref() != Some(&activation.root)
            || session.source_id.as_ref() != Some(&activation.source_id)
            || !activation.registered
        {
            return None;
        }
        Some((
            activation.source_id.clone(),
            activation.lifecycle_generation,
        ))
    }

    pub(super) fn retry_job_audit(&mut self, cx: &mut Context<Self>) {
        let jobs = snapshot(self, cx);
        if !jobs
            .rows
            .iter()
            .any(|row| row.kind == JobKind::Catalog && row.can_retry)
        {
            self.job_action_error = Some("This source audit is no longer ready to retry".into());
            cx.notify();
            return;
        }
        let Some((source_id, lifecycle_generation)) = self.current_job_audit() else {
            self.job_action_error = Some("The source changed before retry".into());
            cx.notify();
            return;
        };
        let result = super::catalog::submit_catalog_command(
            CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation,
            },
            cx,
        );
        match result {
            Ok(()) => {
                self.job_action = Some(JobActionState {
                    source_id,
                    lifecycle_generation,
                    kind: JobActionKind::RetryQueued,
                });
                self.job_action_error = None;
            }
            Err(error) => self.job_action_error = Some(format!("Retry was not accepted: {error}")),
        }
        cx.notify();
    }

    pub(super) fn cancel_job_audit(&mut self, cx: &mut Context<Self>) {
        let jobs = snapshot(self, cx);
        if !jobs
            .rows
            .iter()
            .any(|row| row.kind == JobKind::Catalog && row.can_cancel)
        {
            self.job_action_error = Some("This source audit is no longer cancelable".into());
            cx.notify();
            return;
        }
        let Some((source_id, lifecycle_generation)) = self.current_job_audit() else {
            self.job_action_error = Some("The source changed before cancellation".into());
            cx.notify();
            return;
        };
        let result = if cx.has_global::<SourceRegistryGlobal>() {
            cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
                global.cancel_catalog_audit(&source_id, lifecycle_generation)
            })
        } else {
            Err(yield_session::CatalogCancelError::Closed)
        };
        match result {
            Ok(()) => {
                self.job_action = Some(JobActionState {
                    source_id,
                    lifecycle_generation,
                    kind: JobActionKind::CancelRequested,
                });
                self.job_action_error = None;
            }
            Err(error) => {
                self.job_action_error = Some(format!("Cancellation was not accepted: {error}"));
            }
        }
        cx.notify();
    }
}

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
        assert_eq!(jobs.status_label(), "2 jobs running");
        assert_eq!(jobs.rows[0].generation, 7);
        assert_eq!(jobs.rows[1].generation, 11);
        assert!(jobs.rows.iter().all(|row| row.source_label() == "first"));

        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/sources/second"));
            view.scan_generation = 8;
            view.catalog_activation = Some(catalog("/sources/first", 11, None));
        });
        let jobs = browser.update(cx, |view, cx| snapshot(view, cx));
        assert_eq!(jobs.status_label(), "1 job running");
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
            source: Some(PathBuf::from("/source")),
            generation: 4,
            work: "Retry this source audit",
            blocked: Some("Source unavailable".into()),
            action: None,
            can_retry: true,
            can_cancel: false,
        });
        assert_eq!(snapshot.status_label(), "Job needs attention");
        snapshot.rows.push(JobRow {
            kind: JobKind::Waveform,
            source: Some(PathBuf::from("/source")),
            generation: 8,
            work: "Loading selected waveform",
            blocked: None,
            action: None,
            can_retry: false,
            can_cancel: false,
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
