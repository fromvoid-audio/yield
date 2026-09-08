//! GPUI projection of the profile-owned preference stream.

use std::sync::Arc;
use std::time::Duration;

use gpui::{BorrowAppContext, Context};
use wavecrate_session::{
    PreferenceEvent, PreferencePatch, PreferenceRequestId, PreferenceRevision, SessionError,
    SubmitError, UiPreferences,
};

use super::Browser;
use super::source_registry::SourceRegistryGlobal;

/// One preference patch admitted by the session service but not yet known to be durable.
#[derive(Clone, Debug)]
pub(super) struct PendingPreferencePatch {
    request: u64,
    patch: PreferencePatch,
    applied_revision: Option<u64>,
}

/// The UI's authoritative preference baseline plus ordered optimistic edits.
#[derive(Debug)]
pub(super) struct PreferenceState {
    loaded: bool,
    valid: bool,
    baseline: UiPreferences,
    revision: u64,
    persisted_revision: u64,
    pending: Vec<PendingPreferencePatch>,
    status: String,
    terminal: bool,
    blocked_until_reset: bool,
}

impl Default for PreferenceState {
    fn default() -> Self {
        Self::new()
    }
}

impl PreferenceState {
    pub(super) fn new() -> Self {
        Self {
            loaded: false,
            valid: true,
            baseline: UiPreferences::default(),
            revision: 0,
            persisted_revision: 0,
            pending: Vec::new(),
            status: "Loading settings…".to_string(),
            terminal: false,
            blocked_until_reset: false,
        }
    }

    pub(super) fn loaded(&self) -> bool {
        self.loaded
    }

    pub(super) fn can_edit(&self) -> bool {
        !self.terminal && !self.blocked_until_reset && (!self.loaded || self.valid)
    }

    pub(super) fn can_submit(&self, patch: &PreferencePatch) -> bool {
        self.can_edit()
            || (!self.terminal
                && self.blocked_until_reset
                && matches!(patch, PreferencePatch::Reset))
            || (!self.terminal && !self.valid && matches!(patch, PreferencePatch::Reset))
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(super) fn has_pending_theme_patch(&self) -> bool {
        self.pending.iter().any(|pending| {
            matches!(
                pending.patch,
                PreferencePatch::ReplaceTheme { .. } | PreferencePatch::Reset
            )
        })
    }

    pub(super) fn has_pending_audio_output_patch(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.patch, PreferencePatch::SetAudioOutput(_)))
    }

    pub(super) fn has_pending_reset(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.patch, PreferencePatch::Reset))
    }

    pub(super) fn status(&self) -> &str {
        &self.status
    }

    pub(super) fn terminal(&self) -> bool {
        self.terminal
    }

    pub(super) fn effective(&self) -> UiPreferences {
        let mut preferences = self.baseline.clone();
        for pending in &self.pending {
            apply_patch(&mut preferences, &pending.patch);
        }
        preferences
    }

    pub(super) fn queue(&mut self, request: PreferenceRequestId, patch: PreferencePatch) {
        self.pending.push(PendingPreferencePatch {
            request: request.get(),
            patch,
            applied_revision: None,
        });
        if self
            .pending
            .last()
            .is_some_and(|pending| matches!(&pending.patch, PreferencePatch::Reset))
        {
            self.blocked_until_reset = false;
            self.valid = true;
        }
        self.terminal = false;
        self.status = "Saving settings…".to_string();
    }

    #[cfg(test)]
    fn queue_for_test(&mut self, request: u64, patch: PreferencePatch) {
        self.pending.push(PendingPreferencePatch {
            request,
            patch,
            applied_revision: None,
        });
    }

    pub(super) fn accept_snapshot(
        &mut self,
        revision: PreferenceRevision,
        preferences: UiPreferences,
    ) -> bool {
        if revision.get() < self.revision {
            return false;
        }
        self.loaded = true;
        self.valid = true;
        self.baseline = preferences;
        self.revision = revision.get();
        self.terminal = false;
        self.blocked_until_reset = false;
        self.refresh_status();
        true
    }

    pub(super) fn mark_applied(
        &mut self,
        request: PreferenceRequestId,
        revision: PreferenceRevision,
    ) -> bool {
        let mut found = false;
        for pending in &mut self.pending {
            if pending.request == request.get() {
                pending.applied_revision = Some(revision.get());
                found = true;
                break;
            }
        }
        if revision.get() > self.revision {
            self.revision = revision.get();
        }
        self.retire_persisted();
        self.refresh_status();
        found
    }

    #[cfg(test)]
    fn mark_applied_for_test(&mut self, request: u64, revision: u64) {
        for pending in &mut self.pending {
            if pending.request == request {
                pending.applied_revision = Some(revision);
                break;
            }
        }
        if revision > self.revision {
            self.revision = revision;
        }
        self.retire_persisted();
        self.refresh_status();
    }

    pub(super) fn mark_persisted(&mut self, revision: PreferenceRevision) -> bool {
        if revision.get() < self.persisted_revision {
            return false;
        }
        self.persisted_revision = revision.get();
        self.retire_persisted();
        self.refresh_status();
        true
    }

    #[cfg(test)]
    fn mark_persisted_for_test(&mut self, revision: u64) {
        if revision < self.persisted_revision {
            return;
        }
        self.persisted_revision = revision;
        self.retire_persisted();
        self.refresh_status();
    }

    pub(super) fn fail_request(
        &mut self,
        request: PreferenceRequestId,
        error: String,
    ) -> Option<PreferencePatch> {
        let index = self
            .pending
            .iter()
            .position(|pending| pending.request == request.get())?;
        let failed = self.pending.remove(index).patch;
        self.status = format!("Settings error: {error}");
        Some(failed)
    }

    pub(super) fn load_failed(&mut self, error: String) {
        self.loaded = true;
        self.valid = false;
        self.blocked_until_reset = false;
        self.baseline = UiPreferences::default();
        self.revision = 0;
        self.persisted_revision = 0;
        self.pending.clear();
        self.terminal = false;
        self.status =
            format!("Settings invalid — choose Reset saved settings to recover ({error})");
    }

    pub(super) fn mark_unavailable(&mut self, error: &SessionError) {
        self.freeze_pending();
        self.loaded = true;
        let recoverable = matches!(error, SessionError::Preference(_));
        self.terminal = !recoverable;
        self.blocked_until_reset = recoverable;
        self.status = if recoverable {
            format!("Settings save failed — choose Reset saved settings to retry ({error})")
        } else {
            format!("Settings persistence unavailable: {error}")
        };
    }

    pub(super) fn mark_closed(&mut self) {
        self.freeze_pending();
        self.loaded = true;
        self.terminal = true;
        self.status = "Settings persistence closed".to_string();
    }

    pub(super) fn mark_submission_error(&mut self, error: &SubmitError) {
        self.status = format!("Settings not saved: {error}");
    }

    pub(super) fn mark_edit_blocked(&mut self) {
        if self.terminal {
            return;
        }
        self.status = if self.blocked_until_reset {
            "Settings save failed — choose Reset saved settings to retry".to_string()
        } else {
            "Settings invalid — choose Reset saved settings to recover".to_string()
        };
    }

    fn freeze_pending(&mut self) {
        self.baseline = self.effective();
        self.pending.clear();
    }

    fn retire_persisted(&mut self) {
        let persisted = self.persisted_revision;
        self.pending.retain(|pending| {
            !pending
                .applied_revision
                .is_some_and(|revision| revision <= persisted)
        });
    }

    fn refresh_status(&mut self) {
        if self.terminal {
            return;
        }
        if self.loaded && !self.valid {
            self.status = "Settings invalid — choose Reset saved settings to recover".to_string();
        } else if self.has_pending() {
            self.status = "Saving settings…".to_string();
        } else if self.loaded && self.persisted_revision >= self.revision {
            self.status = "Settings saved".to_string();
        } else if self.loaded {
            self.status = "Settings loaded".to_string();
        } else {
            self.status = "Loading settings…".to_string();
        }
    }
}

fn apply_patch(preferences: &mut UiPreferences, patch: &PreferencePatch) {
    match patch {
        PreferencePatch::ReplaceTheme { theme, source_path } => {
            preferences.theme = theme.clone();
            preferences.theme_source_path = source_path.clone();
        }
        PreferencePatch::SetMotionEnabled(enabled) => preferences.theme.motion.enabled = *enabled,
        PreferencePatch::SetAutoplaySelection(enabled) => preferences.autoplay_selection = *enabled,
        PreferencePatch::SetLoopEnabled(enabled) => preferences.loop_enabled = *enabled,
        PreferencePatch::SetOutputVolume(volume) => {
            preferences.output_volume = *volume;
        }
        PreferencePatch::SetAudioOutput(audio_output) => {
            preferences.audio_output = audio_output.clone();
        }
        PreferencePatch::Reset => *preferences = UiPreferences::default(),
    }
}

impl Browser {
    pub(super) fn start_preferences_poll(&mut self, cx: &mut Context<Self>) {
        if self.preferences_task.is_some() || !cx.has_global::<SourceRegistryGlobal>() {
            return;
        }
        if let Some(error) = cx.global::<SourceRegistryGlobal>().start_error() {
            let error = SessionError::Persistence(error.to_string());
            self.preferences.mark_unavailable(&error);
            self.apply_preference_projection(cx);
            return;
        }
        self.preferences_task = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(100)).await;
                let keep_polling = this
                    .update(cx, |view, cx| view.poll_preferences(cx))
                    .unwrap_or(false);
                if !keep_polling {
                    let _ = this.update(cx, |view, _| view.preferences_task = None);
                    break;
                }
            }
        }));
    }

    fn poll_preferences(&mut self, cx: &mut Context<Self>) -> bool {
        let mut events = Vec::new();
        let _ = cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
            global.drain_preference_events(&mut events)
        });
        let mut changed = false;
        for event in events {
            changed |= self.apply_preference_event(event, cx);
        }
        if changed {
            cx.notify();
        }
        !self.preferences.terminal()
            && (!self.preferences.loaded() || self.preferences.has_pending())
    }

    fn apply_preference_event(&mut self, event: PreferenceEvent, cx: &mut Context<Self>) -> bool {
        match event {
            PreferenceEvent::Loading { .. } => {
                self.preferences_task_pending_status();
                true
            }
            PreferenceEvent::Snapshot {
                revision,
                preferences,
            } => {
                if !self.preferences.accept_snapshot(revision, preferences) {
                    return false;
                }
                self.apply_preference_projection(cx);
                true
            }
            PreferenceEvent::Persisted { revision } => self.preferences.mark_persisted(revision),
            PreferenceEvent::RequestApplied { request, revision } => {
                let changed = self.preferences.mark_applied(request, revision);
                self.apply_preference_projection(cx);
                changed
            }
            PreferenceEvent::RequestFailed { request, error } => {
                let failed = self.preferences.fail_request(request, error.to_string());
                if failed.is_some() {
                    if let Some(failed_patch) = failed.as_ref()
                        && matches!(failed_patch, PreferencePatch::SetAudioOutput(_))
                    {
                        self.audio_output_persistence_failed(error.to_string());
                    }
                    let failed_theme = failed.as_ref().is_some_and(|patch| {
                        matches!(
                            patch,
                            PreferencePatch::ReplaceTheme { .. } | PreferencePatch::Reset
                        )
                    });
                    if failed_theme {
                        let generation = self.theme_load.generation;
                        self.theme_load.reject(generation);
                    }
                    self.apply_preference_projection(cx);
                }
                failed.is_some()
            }
            PreferenceEvent::LoadFailed { error } => {
                self.preferences.load_failed(error.to_string());
                let generation = self.theme_load.generation;
                self.theme_load.reject(generation);
                self.apply_preference_projection(cx);
                true
            }
            PreferenceEvent::PersistenceUnavailable { error, .. } => {
                let audio_output_pending = self.preferences.has_pending_audio_output_patch();
                self.preferences.mark_unavailable(&error);
                if audio_output_pending {
                    self.audio_output_persistence_failed(error.to_string());
                }
                let generation = self.theme_load.generation;
                self.theme_load.reject(generation);
                self.apply_preference_projection(cx);
                true
            }
            PreferenceEvent::Closed => {
                let audio_output_pending = self.preferences.has_pending_audio_output_patch();
                self.preferences.mark_closed();
                if audio_output_pending {
                    self.audio_output_persistence_failed("settings persistence closed".to_string());
                }
                let generation = self.theme_load.generation;
                self.theme_load.reject(generation);
                self.apply_preference_projection(cx);
                true
            }
        }
    }

    fn preferences_task_pending_status(&mut self) {
        if !self.preferences.loaded() {
            self.preferences.status = "Loading settings…".to_string();
        }
    }

    fn apply_preference_projection(&mut self, cx: &mut Context<Self>) {
        let effective = self.preferences.effective();
        let autoplay_was_enabled = self.autoplay_selection;
        self.autoplay_selection = effective.autoplay_selection;
        self.playback_policy
            .apply_preference(effective.loop_enabled);
        self.apply_preference_volume(effective.output_volume);
        self.apply_preference_audio_output(
            &effective.audio_output,
            self.preferences.has_pending_reset(),
            cx,
        );
        if autoplay_was_enabled && !self.autoplay_selection {
            self.cancel_pending_selection_autoplay();
        }
        if self.playback_waiting_for_preferences && self.preferences.loaded() {
            self.playback_waiting_for_preferences = false;
            if let Some(intent) = self.playback_init_intent.clone() {
                self.start_playback_runtime(intent, cx);
            }
        }

        let preserve_inflight_theme =
            self.theme_load.pending_path.is_some() && !self.preferences.has_pending_theme_patch();
        if preserve_inflight_theme {
            return;
        }
        let active_theme = crate::theme::active(cx).clone();
        if active_theme != effective.theme {
            cx.set_global(crate::theme::ActiveTheme(Arc::new(effective.theme.clone())));
            self.search_input
                .update(cx, |input, cx| input.theme_changed(cx));
        }
        if self.theme_load.pending_path.is_none() {
            self.theme_load.active_path = effective.theme_source_path.clone();
            self.theme_status = effective.theme.name.clone();
        }
    }

    pub(super) fn submit_preference_patch(
        &mut self,
        patch: PreferencePatch,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.preferences.can_submit(&patch) {
            self.preferences.mark_edit_blocked();
            return false;
        }
        let result = if cx.has_global::<SourceRegistryGlobal>() {
            cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
                if matches!(&patch, PreferencePatch::Reset) {
                    global.reset_preferences()
                } else {
                    global.patch_preferences(patch.clone())
                }
            })
        } else {
            Err(SubmitError::Closed)
        };
        match result {
            Ok(request) => {
                self.preferences.queue(request, patch);
                self.start_preferences_poll(cx);
                true
            }
            Err(error) => {
                self.preferences.mark_submission_error(&error);
                false
            }
        }
    }

    pub(super) fn persist_motion_preference(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.submit_preference_patch(PreferencePatch::SetMotionEnabled(enabled), cx)
    }

    pub(super) fn persist_autoplay_preference(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.submit_preference_patch(PreferencePatch::SetAutoplaySelection(enabled), cx)
    }

    pub(super) fn persist_loop_preference(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.submit_preference_patch(PreferencePatch::SetLoopEnabled(enabled), cx)
    }

    pub(super) fn preference_status(&self) -> &str {
        self.preferences.status()
    }

    pub(super) fn preferences_editable(&self) -> bool {
        self.preferences.can_edit()
    }

    pub(super) fn mark_preferences_edit_blocked(&mut self, cx: &mut Context<Self>) {
        self.preferences.mark_edit_blocked();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wavecrate_design::{Color, Theme};

    #[test]
    fn optimistic_patches_survive_a_late_full_snapshot() {
        let mut state = PreferenceState::new();
        let mut imported = Theme::default();
        imported.name = "Imported".to_string();
        imported.colors.accent = Color::rgb(1, 2, 3);
        state.queue_for_test(1, PreferencePatch::SetAutoplaySelection(false));
        state.queue_for_test(
            2,
            PreferencePatch::ReplaceTheme {
                theme: imported.clone(),
                source_path: Some(PathBuf::from("/tmp/imported.json")),
            },
        );

        let mut saved = UiPreferences::default();
        saved.theme.name = "Saved".to_string();
        saved.loop_enabled = true;
        assert!(state.accept_snapshot(PreferenceRevision::default(), saved));

        let effective = state.effective();
        assert_eq!(effective.theme, imported);
        assert!(!effective.autoplay_selection);
        assert!(effective.loop_enabled);
    }

    #[test]
    fn persistence_only_retires_applied_requests_at_or_before_ack() {
        let mut state = PreferenceState::new();
        state.queue_for_test(1, PreferencePatch::SetLoopEnabled(true));
        state.queue_for_test(2, PreferencePatch::SetAutoplaySelection(false));
        state.mark_applied_for_test(1, 1);
        state.mark_applied_for_test(2, 2);
        state.mark_persisted(PreferenceRevision::default());
        assert!(state.has_pending());
        state.mark_persisted_for_test(2);
        assert!(!state.has_pending());
    }

    #[test]
    fn invalid_payload_requires_reset_before_normal_patch() {
        let mut state = PreferenceState::new();
        state.load_failed("bad JSON".to_string());
        assert!(!state.can_edit());
        assert!(!state.can_submit(&PreferencePatch::SetLoopEnabled(true)));
        assert!(state.can_submit(&PreferencePatch::Reset));
    }

    #[test]
    fn preference_only_persistence_failure_keeps_reset_recovery_available() {
        let mut state = PreferenceState::new();
        state.mark_unavailable(&SessionError::Preference("write failed".to_string()));
        assert!(!state.terminal());
        assert!(!state.can_edit());
        assert!(!state.can_submit(&PreferencePatch::SetLoopEnabled(true)));
        assert!(state.can_submit(&PreferencePatch::Reset));
        assert!(
            state
                .status()
                .contains("choose Reset saved settings to retry")
        );
    }

    #[test]
    fn audio_output_patch_is_optimistic_until_durability_ack() {
        let mut state = PreferenceState::new();
        let output = wavecrate_session::AudioOutputPreferences {
            host: Some("coreaudio".to_string()),
            device: Some("Built-in Output".to_string()),
            sample_rate: Some(48_000),
            buffer_size: Some(256),
        };
        state.queue_for_test(7, PreferencePatch::SetAudioOutput(output.clone()));
        assert_eq!(state.effective().audio_output, output);
        state.mark_applied_for_test(7, 1);
        assert!(state.has_pending());
        state.mark_persisted_for_test(1);
        assert!(!state.has_pending());
    }

    #[test]
    fn reset_is_visible_as_a_preference_override_for_dirty_output() {
        let mut state = PreferenceState::new();
        state.queue_for_test(
            1,
            PreferencePatch::SetAudioOutput(wavecrate_session::AudioOutputPreferences {
                host: Some("coreaudio".to_string()),
                ..wavecrate_session::AudioOutputPreferences::default()
            }),
        );
        state.queue_for_test(2, PreferencePatch::Reset);
        assert!(state.has_pending_reset());
        assert_eq!(state.effective().audio_output, Default::default());
    }
}
