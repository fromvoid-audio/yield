//! Shift-F1 issue feedback prompt and its staged gateway lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use gpui::{
    AppContext, ClickEvent, Context, FocusHandle, KeyDownEvent, Task, Window, div, prelude::*, px,
};
use yield_issue_gateway::{
    AuthRequestId, CreatedIssue, GatewayClient, GatewayError, SecretToken, TokenStore,
    TokenStoreError,
};

use crate::search_input::{SearchChanged, SearchInput, SearchInputOptions};
use crate::theme::color;
use crate::{multiline_text_input::MultilineTextInput, theme};

use super::Browser;

const POLL_LIMIT: usize = 40;
const AUTH_TIMEOUT: Duration = Duration::from_secs(120);
const BODY_LIMIT: usize = 60_000;
const TITLE_LIMIT: usize = 200;

trait FeedbackService: Send + Sync + 'static {
    fn check(&self) -> Result<Connection, ServiceError>;
    fn auth_start_url(&self, request: &AuthRequestId) -> Result<String, ServiceError>;
    fn poll(&self, request: &AuthRequestId) -> Result<Option<SecretToken>, ServiceError>;
    fn store_token(&self, token: &SecretToken) -> Result<(), ServiceError>;
    fn submit(&self, title: &str, body: &str) -> Result<CreatedIssue, SubmitError>;

    fn poll_and_store(
        &self,
        request: &AuthRequestId,
        cancelled: &AtomicBool,
    ) -> Result<bool, ServiceError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        match self.poll(request)? {
            Some(token) => {
                if cancelled.load(Ordering::Acquire) {
                    return Ok(false);
                }
                self.store_token(&token)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

struct ProductionFeedbackService;

impl FeedbackService for ProductionFeedbackService {
    fn check(&self) -> Result<Connection, ServiceError> {
        GatewayClient::from_environment().map_err(ServiceError::Gateway)?;
        Ok(match TokenStore::load() {
            Ok(Some(_)) => Connection::Connected,
            Ok(None) => Connection::MissingAuth,
            Err(_) => Connection::SecureStoreUnavailable,
        })
    }

    fn auth_start_url(&self, request: &AuthRequestId) -> Result<String, ServiceError> {
        GatewayClient::from_environment()
            .map_err(ServiceError::Gateway)?
            .auth_start_url(request)
            .map(|url| url.to_string())
            .map_err(ServiceError::Gateway)
    }

    fn poll(&self, request: &AuthRequestId) -> Result<Option<SecretToken>, ServiceError> {
        GatewayClient::from_environment()
            .map_err(ServiceError::Gateway)?
            .poll(request)
            .map_err(ServiceError::Gateway)
    }

    fn store_token(&self, token: &SecretToken) -> Result<(), ServiceError> {
        TokenStore::store(token).map_err(|_| ServiceError::SecureStoreUnavailable)
    }

    fn submit(&self, title: &str, body: &str) -> Result<CreatedIssue, SubmitError> {
        let client = GatewayClient::from_environment()
            .map_err(|error| SubmitError::Known(error.to_string()))?;
        let token = TokenStore::load()
            .map_err(|TokenStoreError::Unavailable| {
                SubmitError::Known("Secure token storage is unavailable".into())
            })?
            .ok_or_else(|| {
                SubmitError::Known("Connect GitHub before submitting feedback".into())
            })?;
        client
            .create_issue(&token, title, body)
            .map_err(|error| match error {
                GatewayError::UnknownOutcome => SubmitError::Unknown,
                GatewayError::Unauthorized => SubmitError::Unauthorized,
                _ => SubmitError::Known(error.to_string()),
            })
    }
}

#[derive(Debug)]
enum ServiceError {
    Gateway(GatewayError),
    SecureStoreUnavailable,
}

fn service_error_message(error: ServiceError) -> String {
    match error {
        ServiceError::Gateway(GatewayError::Unauthorized) => {
            "GitHub authorization was denied or expired".into()
        }
        ServiceError::Gateway(error) => error.to_string(),
        ServiceError::SecureStoreUnavailable => "Secure token storage is unavailable".into(),
    }
}

fn auth_poll_cancelled(cancelled: &AtomicBool) -> bool {
    cancelled.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthPollStep {
    Poll,
    TimedOut,
    Cancelled,
}

fn auth_poll_step(cancelled: &AtomicBool, elapsed: Duration) -> AuthPollStep {
    if auth_poll_cancelled(cancelled) {
        AuthPollStep::Cancelled
    } else if elapsed >= AUTH_TIMEOUT {
        AuthPollStep::TimedOut
    } else {
        AuthPollStep::Poll
    }
}

fn button_should_activate(enabled: bool, key: &str, held: bool) -> bool {
    enabled && !held && matches!(key, "enter" | "space" | " ")
}

fn auth_completion_is_current(
    current_generation: u64,
    operation_generation: u64,
    open: bool,
    cancelled: &AtomicBool,
) -> bool {
    current_generation == operation_generation && open && !cancelled.load(Ordering::Acquire)
}

fn submission_can_start(submission: &Submission, acknowledged_unknown: bool) -> bool {
    match submission {
        Submission::Idle | Submission::Failed(_) => true,
        Submission::Unknown => acknowledged_unknown,
        Submission::Pending | Submission::Confirmed(_) => false,
    }
}

fn acknowledge_unknown(submission: &mut Submission, acknowledged: &mut bool) -> bool {
    if *submission != Submission::Unknown {
        return false;
    }
    *acknowledged = true;
    *submission = Submission::Idle;
    true
}

fn close_prompt(open: &mut bool) {
    *open = false;
}

#[derive(Clone, Copy)]
enum FeedbackButtonAction {
    Close,
    Bug,
    Feature,
    ShowDisclosure,
    BeginAuth,
    Submit,
    RetryUnknown,
    NewReport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IssueKind {
    Bug,
    Feature,
}

impl IssueKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Bug => "Bug: ",
            Self::Feature => "FR: ",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Connection {
    Checking,
    Unconfigured,
    MissingAuth,
    Connected,
    SecureStoreUnavailable,
    AuthInProgress,
    AuthFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Submission {
    Idle,
    Pending,
    Confirmed(CreatedIssue),
    Failed(String),
    Unknown,
}

/// Browser-owned UI and operation state for the feedback prompt.
pub(super) struct FeedbackIssueState {
    pub(super) open: bool,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    title: gpui::Entity<SearchInput>,
    body: gpui::Entity<MultilineTextInput>,
    kind: IssueKind,
    connection: Connection,
    submission: Submission,
    service: std::sync::Arc<dyn FeedbackService>,
    disclosure: bool,
    auth_generation: u64,
    auth_cancel: Option<Arc<AtomicBool>>,
    auth_task: Option<Task<()>>,
    scroll_handle: gpui::ScrollHandle,
    _title_subscription: gpui::Subscription,
    _body_subscription: gpui::Subscription,
    submit_generation: u64,
    submit_task: Option<Task<()>>,
    allow_unknown_retry: bool,
    message: Option<String>,
}

impl FeedbackIssueState {
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        let title =
            cx.new(|cx| SearchInput::new_with_options(cx, SearchInputOptions::feedback_title()));
        let body = cx.new(MultilineTextInput::new);
        let title_subscription = cx.subscribe(&title, |_, _, _: &SearchChanged, cx| cx.notify());
        let body_subscription = cx.subscribe(
            &body,
            |_, _, _: &crate::multiline_text_input::Changed, cx| cx.notify(),
        );
        title.read(cx).focus_handle().tab_stop(true);
        Self {
            open: false,
            focus: cx.focus_handle(),
            previous_focus: None,
            title,
            body,
            kind: IssueKind::Bug,
            connection: Connection::Checking,
            submission: Submission::Idle,
            service: std::sync::Arc::new(ProductionFeedbackService),
            disclosure: false,
            auth_generation: 0,
            auth_cancel: None,
            auth_task: None,
            scroll_handle: gpui::ScrollHandle::new(),
            _title_subscription: title_subscription,
            _body_subscription: body_subscription,
            submit_generation: 0,
            submit_task: None,
            allow_unknown_retry: false,
            message: None,
        }
    }

    #[cfg(test)]
    pub(super) fn use_test_service(&mut self) {
        self.service = tests::test_service();
    }
}

impl Browser {
    /// Whether other settings or context controls allow the report to open.
    pub(super) fn feedback_entry_available(&self) -> bool {
        !self.theme_settings_open && !self.audio_settings_open && self.context_menu.is_none()
    }

    /// Toggle the global feedback prompt while preserving focus.
    pub(super) fn toggle_feedback_issue(
        &mut self,
        _: Option<&ClickEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.feedback_issue.open {
            self.close_feedback_issue(window, cx);
        } else if !self.shortcut_help.is_open() && self.feedback_entry_available() {
            self.feedback_issue.previous_focus = window.focused(cx);
            self.cancel_file_drag_with_window(window, cx);
            self.cancel_waveform_gestures(window, cx);
            self.cancel_volume_drag(window, cx);
            self.cancel_column_gesture(cx);
            self.cancel_pane_resize(cx);
            self.source_reorder_drag = None;
            self.feedback_issue.open = true;
            self.feedback_issue.disclosure = false;
            self.feedback_issue.message = None;
            self.feedback_issue.connection = Connection::Checking;
            window.focus(&self.feedback_issue.focus);
            self.check_feedback_connection(cx);
            cx.notify();
        }
    }

    fn close_feedback_issue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.feedback_issue.open {
            return;
        }
        close_prompt(&mut self.feedback_issue.open);
        self.cancel_feedback_auth();
        if let Some(previous) = self.feedback_issue.previous_focus.take() {
            window.focus(&previous);
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    fn cancel_feedback_auth(&mut self) {
        if let Some(cancel) = self.feedback_issue.auth_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.feedback_issue.auth_generation = self.feedback_issue.auth_generation.wrapping_add(1);
        self.feedback_issue.auth_task = None;
    }

    fn check_feedback_connection(&mut self, cx: &mut Context<Self>) {
        self.feedback_issue.auth_generation = self.feedback_issue.auth_generation.wrapping_add(1);
        let generation = self.feedback_issue.auth_generation;
        self.feedback_issue.connection = Connection::Checking;
        let service = self.feedback_issue.service.clone();
        self.feedback_issue.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service.check() })
                .await;
            let _ = this.update(cx, |browser, cx| {
                if browser.feedback_issue.auth_generation != generation
                    || !browser.feedback_issue.open
                {
                    return;
                }
                browser.feedback_issue.auth_task = None;
                browser.feedback_issue.connection = match result {
                    Ok(state) => state,
                    Err(ServiceError::Gateway(GatewayError::Unconfigured)) => {
                        Connection::Unconfigured
                    }
                    Err(ServiceError::SecureStoreUnavailable) => Connection::SecureStoreUnavailable,
                    Err(ServiceError::Gateway(error)) => Connection::AuthFailed(error.to_string()),
                };
                cx.notify();
            });
        }));
    }

    fn begin_feedback_auth(&mut self, cx: &mut Context<Self>) {
        if !self.feedback_issue.open
            || !self.feedback_issue.disclosure
            || !matches!(
                self.feedback_issue.connection,
                Connection::MissingAuth | Connection::AuthFailed(_)
            )
        {
            return;
        }
        self.feedback_issue.disclosure = false;
        self.cancel_feedback_auth();
        let generation = self.feedback_issue.auth_generation;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.feedback_issue.auth_cancel = Some(cancelled.clone());
        self.feedback_issue.connection = Connection::AuthInProgress;
        self.feedback_issue.message = Some("Waiting for GitHub authorization…".into());
        let task_cancelled = cancelled.clone();
        let service = self.feedback_issue.service.clone();
        self.feedback_issue.auth_task = Some(cx.spawn(async move |this, cx| {
            let request_id = AuthRequestId::new();
            let start_service = service.clone();
            let start_request = request_id.clone();
            let url = match cx
                .background_executor()
                .spawn(async move { start_service.auth_start_url(&start_request) })
                .await
            {
                Ok(url) => url,
                Err(error) => {
                    let _ = this.update(cx, |browser, cx| {
                        browser.finish_feedback_auth(
                            generation,
                            task_cancelled.clone(),
                            Err(service_error_message(error)),
                            cx,
                        )
                    });
                    return;
                }
            };
            let _ = this.update(cx, |browser, cx| {
                if browser.feedback_issue.auth_generation == generation
                    && !task_cancelled.load(Ordering::Acquire)
                    && browser.feedback_issue.open
                {
                    cx.open_url(&url);
                }
            });
            let started = Instant::now();
            let mut result = Err("GitHub authorization timed out".to_owned());
            for attempt in 0..POLL_LIMIT {
                match auth_poll_step(&task_cancelled, started.elapsed()) {
                    AuthPollStep::Cancelled => return,
                    AuthPollStep::TimedOut => break,
                    AuthPollStep::Poll => {}
                }
                let poll_service = service.clone();
                let poll_id = request_id.clone();
                let poll_cancelled = task_cancelled.clone();
                let poll = cx
                    .background_executor()
                    .spawn(async move { poll_service.poll_and_store(&poll_id, &poll_cancelled) })
                    .await;
                match poll {
                    Ok(true) => {
                        result = Ok(());
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        result = Err(match error {
                            ServiceError::Gateway(GatewayError::Unauthorized) => {
                                "GitHub authorization was denied or expired".into()
                            }
                            error => service_error_message(error),
                        });
                        break;
                    }
                }
                if task_cancelled.load(Ordering::Acquire) {
                    return;
                }
                if started.elapsed() >= AUTH_TIMEOUT {
                    break;
                }
                let delay = poll_delay(attempt);
                let remaining = AUTH_TIMEOUT.saturating_sub(started.elapsed());
                cx.background_executor().timer(delay.min(remaining)).await;
            }
            if task_cancelled.load(Ordering::Acquire) {
                return;
            }
            let _ = this.update(cx, |browser, cx| {
                browser.finish_feedback_auth(generation, task_cancelled, result, cx)
            });
        }));
    }

    fn finish_feedback_auth(
        &mut self,
        generation: u64,
        cancelled: Arc<AtomicBool>,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if !auth_completion_is_current(
            self.feedback_issue.auth_generation,
            generation,
            self.feedback_issue.open,
            &cancelled,
        ) {
            return;
        }
        self.feedback_issue.auth_task = None;
        self.feedback_issue.auth_cancel = None;
        match result {
            Ok(()) => {
                self.feedback_issue.connection = Connection::Connected;
                self.feedback_issue.message = Some("GitHub connection saved securely".into());
            }
            Err(message) => {
                self.feedback_issue.connection = Connection::AuthFailed(message.clone());
                self.feedback_issue.message = Some(message);
            }
        }
        cx.notify();
    }

    fn submit_feedback_issue(&mut self, cx: &mut Context<Self>) {
        if self.feedback_issue.connection != Connection::Connected
            || !submission_can_start(
                &self.feedback_issue.submission,
                self.feedback_issue.allow_unknown_retry,
            )
        {
            return;
        }
        let draft = self
            .feedback_issue
            .title
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let Some(title) = issue_title(self.feedback_issue.kind, &draft) else {
            self.feedback_issue.message =
                Some("Title must be 3–200 UTF-8 bytes including its type prefix".into());
            cx.notify();
            return;
        };
        let raw_body = self.feedback_issue.body.read(cx).text().to_owned();
        let body = compose_body(&raw_body);
        if body.len() > BODY_LIMIT {
            self.feedback_issue.message =
                Some("Description is too long after adding app diagnostics".into());
            cx.notify();
            return;
        }
        self.feedback_issue.submission = Submission::Pending;
        self.feedback_issue.allow_unknown_retry = false;
        self.feedback_issue.message = Some("Submitting one issue…".into());
        self.feedback_issue.submit_generation =
            self.feedback_issue.submit_generation.wrapping_add(1);
        let generation = self.feedback_issue.submit_generation;
        let service = self.feedback_issue.service.clone();
        self.feedback_issue.submit_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service.submit(&title, &body) })
                .await;
            let _ = this.update(cx, |browser, cx| {
                browser.finish_feedback_submit(generation, result, cx)
            });
        }));
    }

    fn finish_feedback_submit(
        &mut self,
        generation: u64,
        result: Result<CreatedIssue, SubmitError>,
        cx: &mut Context<Self>,
    ) {
        if self.feedback_issue.submit_generation != generation {
            return;
        }
        self.feedback_issue.submit_task = None;
        match result {
            Ok(issue) => {
                self.feedback_issue.submission = Submission::Confirmed(issue.clone());
                self.feedback_issue.message =
                    Some(format!("Created GitHub issue #{}", issue.number));
            }
            Err(SubmitError::Unknown) => {
                self.feedback_issue.submission = Submission::Unknown;
                self.feedback_issue.message = Some("The outcome is unknown. Check https://github.com/PORTALSURFER/yield/issues before retrying.".into());
            }
            Err(SubmitError::Unauthorized) => {
                let message = "GitHub authorization expired. Connect again before submitting.";
                self.feedback_issue.connection = Connection::MissingAuth;
                self.feedback_issue.submission = Submission::Failed(message.into());
                self.feedback_issue.message = Some(message.into());
            }
            Err(SubmitError::Known(message)) => {
                self.feedback_issue.submission = Submission::Failed(message.clone());
                self.feedback_issue.message = Some(message);
            }
        }
        cx.notify();
    }

    fn new_feedback_report(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.feedback_issue.submission, Submission::Confirmed(_)) {
            return;
        }
        self.feedback_issue
            .title
            .update(cx, |title, cx| title.set_content("", cx));
        self.feedback_issue
            .body
            .update(cx, |body, cx| body.set_text("", cx));
        self.feedback_issue.kind = IssueKind::Bug;
        self.feedback_issue.submission = Submission::Idle;
        self.feedback_issue.allow_unknown_retry = false;
        self.feedback_issue.message = None;
        cx.notify();
    }

    fn activate_feedback_button(
        &mut self,
        action: FeedbackButtonAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FeedbackButtonAction::Close => self.close_feedback_issue(window, cx),
            FeedbackButtonAction::Bug => self.feedback_issue.kind = IssueKind::Bug,
            FeedbackButtonAction::Feature => self.feedback_issue.kind = IssueKind::Feature,
            FeedbackButtonAction::ShowDisclosure => self.feedback_issue.disclosure = true,
            FeedbackButtonAction::BeginAuth => self.begin_feedback_auth(cx),
            FeedbackButtonAction::Submit => self.submit_feedback_issue(cx),
            FeedbackButtonAction::RetryUnknown => {
                if acknowledge_unknown(
                    &mut self.feedback_issue.submission,
                    &mut self.feedback_issue.allow_unknown_retry,
                ) {
                    self.feedback_issue.message = Some(
                        "Retry enabled after your check. Submitting again creates another POST."
                            .into(),
                    );
                }
            }
            FeedbackButtonAction::NewReport => self.new_feedback_report(cx),
        }
        cx.notify();
    }

    pub(super) fn handle_feedback_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.feedback_issue.open {
            if event.keystroke.key == "escape" && !event.is_held {
                self.close_feedback_issue(window, cx);
            } else if event.keystroke.key == "tab" && !event.is_held {
                if event.keystroke.modifiers.shift {
                    window.focus_prev();
                } else {
                    window.focus_next();
                }
            }
            cx.stop_propagation();
            return true;
        }
        let mods = event.keystroke.modifiers;
        if !event.is_held
            && event.keystroke.key == "f1"
            && mods.shift
            && !mods.platform
            && !mods.control
            && !mods.alt
        {
            if self.shortcut_help.is_open() {
                self.report_issue_from_shortcut_help(window, cx);
            } else {
                self.toggle_feedback_issue(None, window, cx);
            }
            cx.stop_propagation();
            return true;
        }
        false
    }
}

enum SubmitError {
    Known(String),
    Unknown,
    Unauthorized,
}

fn poll_delay(attempt: usize) -> Duration {
    Duration::from_secs((1u64 << attempt.min(4)).min(10))
}

fn issue_title(kind: IssueKind, draft: &str) -> Option<String> {
    let draft = draft.trim();
    if draft.len() < 3 {
        return None;
    }
    let title = format!("{}{}", kind.prefix(), draft);
    (title.len() <= TITLE_LIMIT).then_some(title)
}

fn compose_body(description: &str) -> String {
    let diagnostics = format!(
        "\n\n---\nYield {} · {} {} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        safe_build_id(option_env!("YIELD_BUILD_ID"))
    );
    format!("{}{}", description.trim(), diagnostics)
}

fn safe_build_id(build_id: Option<&str>) -> String {
    let Some(build_id) = build_id else {
        return "build id unavailable".into();
    };
    let safe: String = build_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || ".-_".contains(*character))
        .take(64)
        .collect();
    if safe.is_empty() {
        "build id unavailable".into()
    } else {
        safe
    }
}

fn connection_label(connection: &Connection) -> &str {
    match connection {
        Connection::Checking => "Checking feedback service and secure connection…",
        Connection::Unconfigured => "Feedback service is not configured on this build.",
        Connection::MissingAuth => "Connect GitHub to submit feedback.",
        Connection::Connected => "GitHub is connected using secure system storage.",
        Connection::SecureStoreUnavailable => "Secure token storage is unavailable.",
        Connection::AuthInProgress => "Waiting for GitHub authorization…",
        Connection::AuthFailed(message) => message,
    }
}

fn submission_label(submission: &Submission) -> Option<String> {
    match submission {
        Submission::Idle => None,
        Submission::Pending => Some("Submission is in progress. Closing this prompt will not cancel it.".into()),
        Submission::Confirmed(issue) => Some(format!("Created issue #{} · {}", issue.number, issue.url)),
        Submission::Failed(message) => Some(message.clone()),
        Submission::Unknown => Some("The issue may have been created. Check https://github.com/PORTALSURFER/yield/issues before retrying.".into()),
    }
}

/// Render the modal prompt. Its state and issued POST outlive closing the overlay.
pub(super) fn render(
    browser: &Browser,
    window_size: gpui::Size<gpui::Pixels>,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let state = &browser.feedback_issue;
    let theme = theme::active(cx).clone();
    let connection = connection_label(&state.connection);
    let issue = submission_label(&state.submission);
    let title = state.title.clone();
    let body = state.body.clone();
    let draft_len = title.read(cx).content().trim().len();
    let title_len = draft_len + state.kind.prefix().len();
    let body_len = compose_body(body.read(cx).text()).len();
    let can_submit = matches!(state.connection, Connection::Connected)
        && !matches!(
            state.submission,
            Submission::Pending | Submission::Confirmed(_)
        )
        && (state.submission != Submission::Unknown || state.allow_unknown_retry)
        && draft_len >= 3
        && title_len <= TITLE_LIMIT
        && body_len <= BODY_LIMIT;
    let kind_enabled = !matches!(
        state.submission,
        Submission::Pending | Submission::Confirmed(_)
    );
    let bg = color(theme.colors.surface_raised);
    let border = color(theme.colors.border_emphasis);
    let accent = color(theme.colors.accent);
    let mut modal = div()
        .id("feedback-issue-modal")
        .track_focus(&state.focus)
        .tab_group()
        .w(px((f32::from(window_size.width) - 48.0).clamp(420.0, 720.0)))
        .max_h(px((f32::from(window_size.height) - 48.0).max(300.0)))
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&state.scroll_handle)
        .p(px(theme.spacing.panel_padding))
        .flex()
        .flex_col()
        .gap(px(theme.spacing.control_gap))
        .bg(bg)
        .border(px(theme.spacing.border_width))
        .border_color(border)
        .occlude()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child("Send feedback to Yield")
                .child(button(
                    "feedback-issue-close",
                    "Close",
                    true,
                    FeedbackButtonAction::Close,
                    cx,
                )),
        );
    modal = modal.child(
        div()
            .flex()
            .gap(px(theme.spacing.control_gap))
            .child(button(
                "feedback-kind-bug",
                if state.kind == IssueKind::Bug {
                    "Bug report · selected"
                } else {
                    "Bug report"
                },
                kind_enabled,
                FeedbackButtonAction::Bug,
                cx,
            ))
            .child(button(
                "feedback-kind-feature",
                if state.kind == IssueKind::Feature {
                    "Feature request · selected"
                } else {
                    "Feature request"
                },
                kind_enabled,
                FeedbackButtonAction::Feature,
                cx,
            )),
    );
    modal = modal
        .child(div().child("Title"))
        .child(title)
        .child(div().text_color(color(theme.colors.text_muted)).child(format!(
            "{title_len} / {TITLE_LIMIT} UTF-8 bytes including prefix; enter at least 3 bytes"
        )))
        .child(div().child("Description (Enter inserts a new line)"))
        .child(body)
        .child(
            div()
                .text_color(color(if body_len > BODY_LIMIT {
                    theme.colors.warning
                } else {
                    theme.colors.text_muted
                }))
                .child(format!(
                    "{body_len} / {BODY_LIMIT} UTF-8 bytes including diagnostics; oversized edits are rejected"
                )),
        )
        .child(
            div()
                .text_color(color(theme.colors.text_muted))
                .child(connection.to_owned()),
        );
    if state.disclosure {
        modal = modal.child(div().p(px(theme.spacing.control_gap)).bg(color(theme.colors.surface_overlay))
            .child("GitHub's repository scope grants access to other private repositories available to the authorizing account. Yield's feedback gateway writes only to PORTALSURFER/yield.")
            .child(button("feedback-auth-confirm", "I understand · open GitHub", true, FeedbackButtonAction::BeginAuth, cx)));
    } else if matches!(
        state.connection,
        Connection::MissingAuth | Connection::AuthFailed(_)
    ) {
        modal = modal.child(button(
            "feedback-auth",
            "Connect GitHub…",
            true,
            FeedbackButtonAction::ShowDisclosure,
            cx,
        ));
    }
    if let Some(message) = state.message.clone() {
        modal = modal.child(
            div()
                .text_color(color(theme.colors.text_muted))
                .child(message),
        );
    }
    if let Some(issue) = issue {
        modal = modal.child(
            div()
                .text_color(color(theme.colors.text_muted))
                .child(issue),
        );
    }
    if state.submission == Submission::Unknown {
        modal = modal.child(button(
            "feedback-unknown-retry",
            "I checked; allow another submission",
            true,
            FeedbackButtonAction::RetryUnknown,
            cx,
        ));
    }
    modal = modal.child(button(
        "feedback-submit",
        if state.submission == Submission::Pending {
            "Submitting…"
        } else {
            "Submit feedback"
        },
        can_submit,
        FeedbackButtonAction::Submit,
        cx,
    ));
    if let Submission::Confirmed(issue) = &state.submission {
        modal = modal
            .child(
                div()
                    .id("feedback-issue-url")
                    .text_color(accent)
                    .child(issue.url.to_string()),
            )
            .child(button(
                "feedback-new-report",
                "New report",
                true,
                FeedbackButtonAction::NewReport,
                cx,
            ));
    }
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000088))
        .child(modal)
}

fn button(
    id: &'static str,
    label: impl Into<String>,
    enabled: bool,
    action: FeedbackButtonAction,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    div()
        .id(id)
        .tab_stop(true)
        .when(enabled, |element| element.cursor_pointer())
        .p(px(6.0))
        .border(px(1.0))
        .border_color(gpui::hsla(0.0, 0.0, 0.5, 0.5))
        .child(label.into())
        .when(!enabled, |element| element.opacity(0.45))
        .on_click(cx.listener(move |browser, _: &ClickEvent, window, cx| {
            if enabled {
                browser.activate_feedback_button(action, window, cx);
            }
        }))
        .on_key_down(
            cx.listener(move |browser, event: &KeyDownEvent, window, cx| {
                if button_should_activate(enabled, &event.keystroke.key, event.is_held) {
                    browser.activate_feedback_button(action, window, cx);
                    cx.stop_propagation();
                }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockService {
        polls: Mutex<VecDeque<Result<Option<SecretToken>, ServiceError>>>,
        cancel_on_poll: Mutex<Option<Arc<AtomicBool>>>,
        poll_count: Mutex<usize>,
        stored: Mutex<usize>,
        submissions: Mutex<usize>,
    }

    impl FeedbackService for MockService {
        fn check(&self) -> Result<Connection, ServiceError> {
            Ok(Connection::MissingAuth)
        }
        fn auth_start_url(&self, _: &AuthRequestId) -> Result<String, ServiceError> {
            Ok("https://gateway.example/auth/start".into())
        }
        fn poll(&self, _: &AuthRequestId) -> Result<Option<SecretToken>, ServiceError> {
            *self.poll_count.lock().unwrap() += 1;
            let result = self.polls.lock().unwrap().pop_front().unwrap_or(Ok(None));
            if let Some(cancelled) = self.cancel_on_poll.lock().unwrap().take() {
                cancelled.store(true, Ordering::Release);
            }
            result
        }
        fn store_token(&self, _: &SecretToken) -> Result<(), ServiceError> {
            *self.stored.lock().unwrap() += 1;
            Ok(())
        }
        fn submit(&self, _: &str, _: &str) -> Result<CreatedIssue, SubmitError> {
            *self.submissions.lock().unwrap() += 1;
            Err(SubmitError::Unknown)
        }
    }

    #[cfg(test)]
    pub(super) fn test_service() -> std::sync::Arc<dyn FeedbackService> {
        std::sync::Arc::new(MockService::default())
    }

    #[test]
    fn title_validates_draft_then_prefix_utf8_bytes() {
        assert_eq!(
            issue_title(IssueKind::Bug, "  fix  "),
            Some("Bug: fix".into())
        );
        assert_eq!(issue_title(IssueKind::Feature, "  "), None);
        assert_eq!(issue_title(IssueKind::Bug, &"é".repeat(100)), None);
    }

    #[test]
    fn diagnostics_are_bounded_and_build_identifier_is_sanitized() {
        assert_eq!(safe_build_id(None), "build id unavailable");
        assert_eq!(safe_build_id(Some("../private/token")), "..privatetoken");
        assert!(safe_build_id(Some(&"a".repeat(200))).len() <= 64);
        let body = compose_body("  report  ");
        assert!(body.starts_with("report\n\n---\nYield "));
        assert!(body.contains(std::env::consts::OS));
        assert!(body.contains(std::env::consts::ARCH));
    }
    #[test]
    fn polling_delay_is_bounded_and_backoff_capped() {
        assert_eq!(poll_delay(0), Duration::from_secs(1));
        assert_eq!(poll_delay(1), Duration::from_secs(2));
        assert_eq!(poll_delay(39), Duration::from_secs(10));
        assert_eq!(POLL_LIMIT, 40);
        assert_eq!(AUTH_TIMEOUT, Duration::from_secs(120));
        assert_eq!(
            auth_poll_step(&AtomicBool::new(false), AUTH_TIMEOUT),
            AuthPollStep::TimedOut
        );
        assert_eq!(
            auth_poll_step(&AtomicBool::new(true), Duration::ZERO),
            AuthPollStep::Cancelled
        );
        assert_eq!(
            auth_poll_step(&AtomicBool::new(false), Duration::ZERO),
            AuthPollStep::Poll
        );
    }

    #[test]
    fn button_keyboard_activation_respects_enabled_and_repeat_state() {
        assert!(button_should_activate(true, "enter", false));
        assert!(button_should_activate(true, "space", false));
        assert!(!button_should_activate(false, "enter", false));
        assert!(!button_should_activate(true, "enter", true));
        assert!(!button_should_activate(true, "escape", false));
    }

    #[test]
    fn mock_auth_poll_stores_token_without_real_keyring_and_obeys_cancel_fence() {
        let token = SecretToken::new("0123456789abcdef0123456789abcdef".into()).unwrap();
        let service = MockService::default();
        service
            .polls
            .lock()
            .unwrap()
            .extend([Ok(None), Ok(Some(token))]);
        let request = AuthRequestId::new();
        let not_cancelled = AtomicBool::new(false);
        assert!(!service.poll_and_store(&request, &not_cancelled).unwrap());
        assert!(service.poll_and_store(&request, &not_cancelled).unwrap());
        assert_eq!(*service.poll_count.lock().unwrap(), 2);
        assert_eq!(*service.stored.lock().unwrap(), 1);
        let cancelled = AtomicBool::new(true);
        assert!(!service.poll_and_store(&request, &cancelled).unwrap());
        assert!(!auth_completion_is_current(
            2,
            1,
            true,
            &AtomicBool::new(false)
        ));
        assert!(!auth_completion_is_current(
            2,
            2,
            false,
            &AtomicBool::new(false)
        ));
    }

    #[test]
    fn cancellation_during_poll_prevents_secure_store_write() {
        let token = SecretToken::new("0123456789abcdef0123456789abcdef".into()).unwrap();
        let service = MockService::default();
        service.polls.lock().unwrap().push_back(Ok(Some(token)));
        let cancelled = Arc::new(AtomicBool::new(false));
        *service.cancel_on_poll.lock().unwrap() = Some(cancelled.clone());
        let request = AuthRequestId::new();
        assert!(!service.poll_and_store(&request, &cancelled).unwrap());
        assert_eq!(*service.stored.lock().unwrap(), 0);
        assert_eq!(
            auth_poll_step(&cancelled, Duration::ZERO),
            AuthPollStep::Cancelled
        );
    }

    #[test]
    fn mock_submission_posts_once_and_pending_or_unknown_state_survives_close() {
        let service = MockService::default();
        let mut submission = Submission::Idle;
        let acknowledged = false;
        if submission_can_start(&submission, acknowledged) {
            submission = Submission::Pending;
            let _ = service.submit("Bug: broken", "details");
        }
        if submission_can_start(&submission, acknowledged) {
            let _ = service.submit("Bug: broken", "details");
        }
        assert_eq!(*service.submissions.lock().unwrap(), 1);
        let mut open = true;
        close_prompt(&mut open);
        assert!(!open);
        assert_eq!(submission, Submission::Pending);

        submission = Submission::Unknown;
        assert!(!submission_can_start(&submission, false));
        let mut acknowledged = false;
        assert!(acknowledge_unknown(&mut submission, &mut acknowledged));
        assert!(submission_can_start(&submission, acknowledged));
        assert!(
            submission_label(&Submission::Unknown)
                .unwrap()
                .contains("Check")
        );
    }

    #[gpui::test]
    fn closed_submission_completion_survives_reopen_until_new_report(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let issue = CreatedIssue {
            url: "https://github.com/PORTALSURFER/yield/issues/42"
                .parse()
                .unwrap(),
            number: 42,
        };
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.feedback_issue.use_test_service();
                view.feedback_issue.open = true;
                view.feedback_issue.submission = Submission::Pending;
                view.feedback_issue.submit_generation = 4;
                view.feedback_issue
                    .title
                    .update(cx, |title, cx| title.set_content("Something broke", cx));
                view.feedback_issue
                    .body
                    .update(cx, |body, cx| body.set_text("Please fix this", cx));
                view.close_feedback_issue(window, cx);
                assert!(!view.feedback_issue.open);
            });
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.finish_feedback_submit(4, Ok(issue.clone()), cx);
                assert_eq!(view.feedback_issue.submission, Submission::Confirmed(issue));
                view.toggle_feedback_issue(None, window, cx);
                assert!(view.feedback_issue.open);
                assert!(matches!(
                    view.feedback_issue.submission,
                    Submission::Confirmed(_)
                ));
                view.new_feedback_report(cx);
                assert_eq!(view.feedback_issue.submission, Submission::Idle);
                assert_eq!(view.feedback_issue.title.read(cx).content(), "");
                assert_eq!(view.feedback_issue.body.read(cx).text(), "");
                view.feedback_issue.submission = Submission::Unknown;
                view.new_feedback_report(cx);
                assert_eq!(view.feedback_issue.submission, Submission::Unknown);
            });
        });
    }
}
