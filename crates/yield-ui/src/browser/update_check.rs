//! Actual settings renderer and one fenced background update metadata request.
use super::Browser;
use crate::update_check::{
    UpdateCheckOutcome, UpdateCheckRequest, UpdateCheckService, UpdateVersion,
};
use gpui::{Context, Div, div, prelude::*, px};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use yield_design::Theme;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum State {
    #[default]
    Idle,
    Checking,
    Current,
    Newer(UpdateVersion, u64),
    Offline,
    Failed,
    Mismatch,
    Unavailable,
}

impl State {
    fn message(self) -> String {
        match self {
            Self::Idle => "Updates have not been checked.".into(),
            Self::Checking => "Checking for updates…".into(),
            Self::Current => "Yield is up to date on this release channel.".into(),
            Self::Newer(version, build) => format!("Yield {version} (build {build}) is available. Automatic installation is unavailable."),
            Self::Offline => "Could not check updates while offline. Reconnect and try again.".into(),
            Self::Failed => "Update check failed. Try again; the installed app is unchanged.".into(),
            Self::Mismatch => "Release metadata did not match this app. Try again; the installed app is unchanged.".into(),
            Self::Unavailable => "Update checking is unavailable for this app.".into(),
        }
    }
}

#[derive(Default)]
pub(super) struct UpdateCheck {
    generation: u64,
    state: State,
    task: Option<gpui::Task<()>>,
    cancellation: Option<Cancellation>,
    service: Option<UpdateCheckService>,
}

struct Cancellation(Arc<AtomicBool>);
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl UpdateCheck {
    fn projected_state(&self, current: Option<&UpdateCheckService>) -> State {
        let same = current
            .zip(self.service.as_ref())
            .is_some_and(|(current, old)| {
                current.channel == old.channel
                    && current.platform == old.platform
                    && current.installed_build == old.installed_build
                    && current.installed_version == old.installed_version
                    && Arc::ptr_eq(&current.checker, &old.checker)
            });
        if current.is_none() || (self.state != State::Idle && !same) {
            State::Unavailable
        } else {
            self.state
        }
    }

    pub(super) fn close(&mut self) {
        if let Some(cancel) = &self.cancellation {
            cancel.0.store(true, Ordering::Release);
        }
        // Do not release the inflight task slot until its background worker returns.
        self.state = State::Idle;
    }

    fn finish(
        &mut self,
        generation: u64,
        request: UpdateCheckRequest,
        outcome: UpdateCheckOutcome,
    ) -> bool {
        if generation != self.generation
            || self.state != State::Checking
            || self
                .cancellation
                .as_ref()
                .is_some_and(|c| c.0.load(Ordering::Acquire))
        {
            return false;
        }
        self.state = match outcome {
            UpdateCheckOutcome::Candidate { .. } if !request.build.known() => State::Unavailable,
            UpdateCheckOutcome::Candidate {
                version,
                channel,
                platform,
                build,
            } if channel != request.channel
                || platform != request.platform
                || !yield_platform::update_catalog::release_core_is_admissible(
                    [
                        request.installed.0,
                        request.installed.1,
                        request.installed.2,
                    ],
                    [version.0, version.1, version.2],
                )
                || !build.known() =>
            {
                State::Mismatch
            }
            UpdateCheckOutcome::Candidate { version, build, .. }
                if build == request.build && version == request.installed =>
            {
                State::Current
            }
            UpdateCheckOutcome::Candidate { version, build, .. }
                if build.commit != request.build.commit && build.number > request.build.number =>
            {
                State::Newer(version, build.number)
            }
            UpdateCheckOutcome::Candidate { .. } => State::Mismatch,
            UpdateCheckOutcome::Offline => State::Offline,
            UpdateCheckOutcome::Failed => State::Failed,
            UpdateCheckOutcome::Unavailable => State::Unavailable,
        };
        true
    }
}

impl Browser {
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if !self.theme_settings_open || self.general_settings.updates.task.is_some() {
            return;
        }
        let updates = &mut self.general_settings.updates;
        let Some(service) = cx.try_global::<UpdateCheckService>().cloned() else {
            updates.state = State::Unavailable;
            cx.notify();
            return;
        };
        let Some(generation) = updates.generation.checked_add(1) else {
            updates.state = State::Unavailable;
            cx.notify();
            return;
        };
        let request = UpdateCheckRequest {
            installed: service.installed_version,
            channel: service.channel,
            platform: service.platform,
            build: service.installed_build,
        };
        if !request.build.known() {
            updates.state = State::Unavailable;
            cx.notify();
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        updates.generation = generation;
        updates.state = State::Checking;
        updates.cancellation = Some(Cancellation(cancel.clone()));
        updates.service = Some(service.clone());
        let checker = service.checker.clone();
        updates.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if cancel.load(Ordering::Acquire) {
                        return UpdateCheckOutcome::Unavailable;
                    }
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        service.checker.check(request, &cancel)
                    }))
                    .unwrap_or(UpdateCheckOutcome::Failed)
                })
                .await;
            let _ = this.update(cx, |browser, cx| {
                let same_service = cx
                    .try_global::<UpdateCheckService>()
                    .is_some_and(|current| {
                        current.channel == request.channel
                            && current.platform == request.platform
                            && current.installed_build == request.build
                            && current.installed_version == request.installed
                            && Arc::ptr_eq(&current.checker, &checker)
                    });
                let updates = &mut browser.general_settings.updates;
                if generation != updates.generation {
                    return;
                }
                updates.task = None;
                if browser.theme_settings_open {
                    updates.finish(
                        generation,
                        request,
                        if same_service {
                            result
                        } else {
                            UpdateCheckOutcome::Unavailable
                        },
                    );
                }
                updates.cancellation = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_update_check(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let updates = &self.general_settings.updates;
        let busy = updates.task.is_some();
        let service = cx.try_global::<UpdateCheckService>();
        let available = service.is_some();
        let state = updates.projected_state(service);
        let message = if busy && state != State::Checking {
            "Waiting for the previous update check to finish.".into()
        } else {
            state.message()
        };
        div()
            .id("update-check-panel")
            .debug_selector(|| "update-check-panel".to_owned())
            .flex()
            .flex_col()
            .min_w(px(0.0))
            .w_full()
            .gap(px(theme.spacing.control_gap))
            .child(div().id("update-check-status").w_full().child(message))
            .child(
                super::general_settings::control(
                    self,
                    "check-for-updates",
                    if busy {
                        "Checking…"
                    } else {
                        "Check for Updates"
                    },
                    available && !busy,
                    theme,
                )
                .when(available && !busy, |button| {
                    button.on_click(cx.listener(|view, _, _, cx| view.check_for_updates(cx)))
                }),
            )
    }
}

#[cfg(test)]
mod tests;
