//! Settings consume small original-owner observations, never ZIP allocations or install authority.
use super::Browser;
use crate::update_payload_service::UpdatePayloadService;
use gpui::{Context, div, prelude::*, px};
use std::{sync::Arc, time::Duration};
pub(super) const DOWNLOAD_ID: &str = "verify-update-download";
pub(super) const RETIRE_ID: &str = "retire-update-download";
pub(super) const STATUS_ID: &str = "update-payload-status";
use yield_design::Theme;
use yield_platform::payload_selection::{
    PayloadStageAttempt, PayloadStageObservation, PayloadStagePhase as Phase,
    SelectionCancellation, SelectionError,
};

#[derive(Default)]
pub(super) struct UpdatePayload {
    generation: u64,
    // Strong identity survives counter reuse after replacement; copied integers cannot bind a task.
    request: Option<Arc<u64>>,
    observation: Option<PayloadStageObservation>,
    attempt: Option<PayloadStageAttempt>,
    service: Option<UpdatePayloadService>,
    task: Option<gpui::Task<()>>,
}
impl Drop for UpdatePayload {
    fn drop(&mut self) {
        self.close();
    }
}
impl UpdatePayload {
    pub(super) fn close(&mut self) {
        if let Some(attempt) = &self.attempt {
            attempt.retire();
            self.observation = attempt.observe().ok();
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.attempt.is_some() || self.task.is_some()
    }
    fn projected(
        &self,
        current: Option<&UpdatePayloadService>,
        panel_live: bool,
    ) -> Option<PayloadStageObservation> {
        if let Some(attempt) = &self.attempt {
            let same = current
                .zip(self.service.as_ref())
                .is_some_and(|(a, b)| a.same(b));
            if !same || !panel_live || self.request.as_ref().is_none_or(|r| **r != self.generation)
            {
                attempt.retire();
            }
            let observation = attempt.observe().ok()?;
            if observation.phase == Phase::Staged
                && self
                    .service
                    .as_ref()
                    .is_none_or(|service| !matches_stage(service, attempt, &observation))
            {
                attempt.retire();
                return attempt.observe().ok();
            }
            return Some(observation);
        }
        self.observation.clone()
    }
    pub(super) fn phase(&self) -> Option<Phase> {
        self.observation.as_ref().map(|o| o.phase)
    }
    pub(super) fn message(
        &self,
        current: Option<&UpdatePayloadService>,
        panel_live: bool,
    ) -> String {
        let observation = self.projected(current, panel_live);
        let Some(observation) = observation else {
            return if current.is_some() {
                "No update download is staged.".into()
            } else {
                "Update downloads are unavailable for this app.".into()
            };
        };
        match observation.phase {
            Phase::Selecting => "Selecting a signed update for this release channel…".into(),
            Phase::Downloading => "Downloading update files…".into(),
            Phase::Verifying => "Verifying the downloaded update…".into(),
            Phase::Staged => match observation.identity {
                Some(identity) => format!(
                    "Yield {} (build {}) is verified and retained until this panel closes. Installation and relaunch are unavailable.",
                    identity.version, identity.build_number
                ),
                None => "Update download verification failed.".into(),
            },
            Phase::Stopping => {
                "Stopping the update download; waiting for its data to retire.".into()
            }
            Phase::Unstaged => "No update download is staged.".into(),
            Phase::Offline => {
                "Could not download the update while offline. Reconnect and try again.".into()
            }
            Phase::Failed => {
                "Update download verification failed. The installed app is unchanged.".into()
            }
            Phase::Unavailable => {
                "Update downloads are unavailable; previous data may still be retiring.".into()
            }
        }
    }
}
fn empty(phase: Phase) -> PayloadStageObservation {
    PayloadStageObservation {
        phase,
        identity: None,
        archive_len: None,
        archive_sha256: None,
        build_date: None,
    }
}
fn matches_stage(
    service: &UpdatePayloadService,
    attempt: &PayloadStageAttempt,
    observation: &PayloadStageObservation,
) -> bool {
    service.context.owner.owns(attempt)
        && observation.identity.as_ref().is_some_and(|identity| {
            identity.channel == service.channel
                && identity.platform == service.context.installed.platform
        })
}
fn terminal(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Unstaged | Phase::Offline | Phase::Failed | Phase::Unavailable
    )
}
impl Browser {
    pub(super) fn payload_control_enabled(&self, id: &str, cx: &Context<Self>) -> Option<bool> {
        match id {
            DOWNLOAD_ID => Some(
                self.theme_settings_open
                    && cx.try_global::<UpdatePayloadService>().is_some()
                    && !self.general_settings.payload.busy(),
            ),
            RETIRE_ID => Some(self.theme_settings_open && self.general_settings.payload.busy()),
            _ => None,
        }
    }
    pub(super) fn perform_payload_action(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if self.payload_control_enabled(id, cx) != Some(true) {
            return false;
        }
        match id {
            DOWNLOAD_ID => self.stage_update_payload(cx),
            RETIRE_ID => self.retire_update_payload(cx),
            _ => return false,
        }
        true
    }
    pub(super) fn stage_update_payload(&mut self, cx: &mut Context<Self>) {
        if !self.theme_settings_open || self.general_settings.payload.busy() {
            return;
        }
        let Some(service) = cx.try_global::<UpdatePayloadService>().cloned() else {
            self.general_settings.payload.observation = Some(empty(Phase::Unavailable));
            cx.notify();
            return;
        };
        let Some(generation) = self.general_settings.payload.generation.checked_add(1) else {
            return;
        };
        let attempt = match service
            .context
            .owner
            .start(service.channel, SelectionCancellation::default())
        {
            Ok(attempt) => attempt,
            Err(error) => {
                self.general_settings.payload.observation = Some(empty(match error {
                    SelectionError::Offline => Phase::Offline,
                    SelectionError::Invalid => Phase::Failed,
                    _ => Phase::Unavailable,
                }));
                cx.notify();
                return;
            }
        };
        let payload = &mut self.general_settings.payload;
        let request = Arc::new(generation);
        payload.request = Some(request.clone());
        payload.generation = generation;
        payload.observation = attempt.observe().ok();
        payload.attempt = Some(attempt.clone());
        payload.service = Some(service.clone());
        payload.task = Some(cx.spawn(async move |this, cx| {
            loop {
                // A small original-owner observation; HTTP, cryptography, retained bytes and
                // deallocation remain on the standing background owner, including after stale UI.
                let keep = this.update(cx, |browser, cx| {
                    let current = cx.try_global::<UpdatePayloadService>();
                    let same = current.is_some_and(|current| current.same(&service));
                    let payload = &mut browser.general_settings.payload;
                    if payload.generation != generation
                        || payload
                            .request
                            .as_ref()
                            .is_none_or(|current| !Arc::ptr_eq(current, &request))
                    {
                        attempt.retire();
                        return false;
                    }
                    if !same || !browser.theme_settings_open {
                        attempt.retire();
                    }
                    let Ok(mut observation) = attempt.observe() else {
                        attempt.retire();
                        return true;
                    };
                    if observation.phase == Phase::Staged
                        && (!same
                            || !browser.theme_settings_open
                            || !matches_stage(&service, &attempt, &observation))
                    {
                        attempt.retire();
                        observation = attempt.observe().unwrap_or_else(|_| empty(Phase::Stopping));
                    }
                    let done = terminal(observation.phase);
                    if payload.observation.as_ref() != Some(&observation) {
                        payload.observation = Some(observation);
                        cx.notify();
                    }
                    if done {
                        payload.attempt = None;
                        payload.task = None;
                        payload.service = None;
                        payload.request = None;
                    }
                    !done
                });
                match keep {
                    Ok(true) => {}
                    _ => {
                        attempt.retire();
                        break;
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }));
        cx.notify();
    }
    pub(super) fn retire_update_payload(&mut self, cx: &mut Context<Self>) {
        self.general_settings.payload.close();
        cx.notify();
    }
    pub(super) fn render_update_payload(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let payload = &self.general_settings.payload;
        let service = cx.try_global::<UpdatePayloadService>();
        let busy = payload.busy();
        div()
            .id("update-payload-panel")
            .debug_selector(|| "update-payload-panel".into())
            .flex()
            .flex_col()
            .w_full()
            .gap(px(theme.spacing.control_gap))
            .child(
                div()
                    .id(STATUS_ID)
                    .relative()
                    .when_some(self.ax_marker(STATUS_ID), |status, marker| {
                        status.child(marker)
                    })
                    .child(payload.message(service, self.theme_settings_open)),
            )
            .child(
                super::general_settings::control(
                    self,
                    DOWNLOAD_ID,
                    "Download and Verify Update",
                    service.is_some() && !busy,
                    theme,
                )
                .when(service.is_some() && !busy, |button| {
                    button.on_click(cx.listener(|view, _, _, cx| {
                        view.perform_payload_action(DOWNLOAD_ID, cx);
                    }))
                }),
            )
            .child(
                super::general_settings::control(
                    self,
                    RETIRE_ID,
                    if payload.phase() == Some(Phase::Staged) {
                        "Discard Download"
                    } else {
                        "Cancel Download"
                    },
                    busy,
                    theme,
                )
                .when(busy, |button| {
                    button.on_click(cx.listener(|view, _, _, cx| {
                        view.perform_payload_action(RETIRE_ID, cx);
                    }))
                }),
            )
    }
}
#[cfg(test)]
mod tests;
