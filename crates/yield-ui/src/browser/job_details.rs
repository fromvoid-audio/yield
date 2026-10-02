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
    FeatureAnalysis,
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
            Self::FeatureAnalysis => "Selected-file features",
        }
    }
}

/// Feature presentation is distinct from the worker's retained result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FeatureJobState {
    Preparing,
    Queued,
    Running,
    Blocked,
    NeedsRevalidation,
    Ready,
}

impl FeatureJobState {
    fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing · time remaining unknown",
            Self::Queued => "Queued · awaiting feature work",
            Self::Running => "Running · time remaining unknown",
            Self::Blocked => "Blocked · needs attention",
            Self::NeedsRevalidation => "Needs revalidation · source updated",
            Self::Ready => "Selected file ready",
        }
    }
}

/// A bounded current presentation row. Feature work carries phase target counts;
/// other workers and compute phases remain explicitly indeterminate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JobRow {
    pub(super) kind: JobKind,
    pub(super) feature_state: Option<FeatureJobState>,
    pub(super) source: Option<PathBuf>,
    pub(super) generation: u64,
    pub(super) work: String,
    pub(super) completed: bool,
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
        } else if let Some(state) = self.feature_state {
            state.label()
        } else if self.completed {
            "Selected file ready"
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

pub(super) fn bounded_text(value: &str, max_chars: usize) -> String {
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
        self.rows
            .iter()
            .filter(|row| row.blocked.is_none() && !row.completed)
            .count()
    }

    pub(super) fn status_label(&self) -> String {
        let count = self.active_count();
        if count == 1
            && let Some(row) = self.rows.iter().find(|row| {
                row.kind == JobKind::FeatureAnalysis && row.blocked.is_none() && !row.completed
            })
        {
            format!("{} · {}", row.source_label(), row.work)
        } else if count > 0 {
            format!("{count} job{} active", if count == 1 { "" } else { "s" })
        } else if self.rows.iter().any(|row| row.blocked.is_some()) {
            "Job needs attention".into()
        } else {
            "Jobs idle".into()
        }
    }
}

pub(super) fn snapshot(browser: &Browser, cx: &Context<Browser>) -> JobSnapshot {
    let mut rows = Vec::with_capacity(5);
    if let Some(row) = browser.feature_job_row() {
        rows.push(row);
    }
    let active_root = browser.root.as_ref();
    if browser.scan_cancel.is_some()
        && let Some(root) = active_root
    {
        rows.push(JobRow {
            kind: JobKind::SourceScan,
            feature_state: None,
            source: Some(root.clone()),
            generation: browser.scan_generation,
            work: "Finding audio files".into(),
            completed: false,
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
                feature_state: None,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Checking source catalog".into(),
                completed: false,
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
                feature_state: None,
                source: Some(activation.root.clone()),
                generation: activation.lifecycle_generation,
                work: "Retry this source audit".into(),
                completed: false,
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
            feature_state: None,
            source: active_root.cloned(),
            generation: browser.waveform_generation,
            work: "Loading selected waveform".into(),
            completed: false,
            blocked: None,
            action: None,
            can_retry: false,
            can_cancel: false,
        });
    }
    if browser.audio_output_open_task.is_some() && browser.playback_init_intent.is_some() {
        rows.push(JobRow {
            kind: JobKind::AudioOutput,
            feature_state: None,
            source: active_root.cloned(),
            generation: browser.playback_init_generation,
            work: "Starting audio output".into(),
            completed: false,
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
include!("job_details_tests.rs");
