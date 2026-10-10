//! General settings controls backed by the ordered session preferences.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::TryRecvError,
    time::Duration,
};

use gpui::{
    ClickEvent, ClipboardItem, Context, Div, FocusHandle, PathPromptOptions, Window, div,
    prelude::*, px,
};
use yield_design::Theme;
use yield_session::{
    DEFAULT_RATING_DECAY_WEEKS, DEFAULT_TEXT_SCALE_PERCENT, ENLARGED_TEXT_SCALE_PERCENT,
    MAX_ANTI_CLICK_FADE_MS, MAX_RATING_DECAY_WEEKS, MAX_USER_SHADER_SOURCE_BYTES,
    MAX_VISUAL_EFFECT_STRENGTH_PERCENT, MIN_RATING_DECAY_WEEKS, PreferencePatch,
    ShippedVisualEffectPreferences, ShippedVisualEffectPreset, UiPreferences,
    UserShaderPreferences, user_shader_content_fingerprint,
};

use super::{Browser, source_registry::SourceRegistryGlobal};
use crate::theme::color;
use yield_session::{CacheClearOutcome, GlobalStorageUsage, MaintenanceRequest};

#[cfg(target_os = "macos")]
use gpui::PreparedPostShaderChain;

#[cfg(target_os = "macos")]
use super::user_shader_runtime::{PreparationPermit, validate_user_shader_abi};

pub(super) struct GeneralSettings {
    pub(super) updates: super::update_check::UpdateCheck,
    pub(super) payload: super::update_payload::UpdatePayload,
    pub(super) text_scale_percent: u16,
    pub(super) rating_decay_weeks: u16,
    pub(super) trash_folder: Option<PathBuf>,
    pub(super) previous_focus: Option<FocusHandle>,
    picker_generation: u64,
    picking: bool,
    status: String,
    storage: Option<GlobalStorageUsage>,
    maintenance_status: String,
    maintenance: Option<PendingMaintenance>,
    maintenance_task: Option<gpui::Task<()>>,
    pub(super) shipped_visual_effect: ShippedVisualEffectPreferences,
    pub(super) visual_effect_local_enabled: Option<bool>,
    visual_effect_last_attempt: Option<ShippedVisualEffectPreferences>,
    visual_effect_debug_override: bool,
    visual_effect_debug_revision: u64,
    pub(super) visual_effect_status: String,
    pub(super) visual_effect_persistence_status: String,
    user_shader_import: UserShaderImport,
    pub(super) user_shader_status: String,
    pub(super) reviewed_user_shader_fingerprint: Option<String>,
    #[cfg(target_os = "macos")]
    user_shader_runtime: UserShaderRuntime,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct UserShaderRuntime {
    generation: u64,
    task: Option<gpui::Task<()>>,
    active_fingerprint: Option<String>,
    attempted: Option<(String, u64)>,
    locally_disabled: bool,
    owns_renderer: bool,
    suppress_shipped: bool,
    debug_revision: u64,
    authority_paused: bool,
    status: String,
}

#[cfg(target_os = "macos")]
struct PreparedUserShader {
    fingerprint: String,
    chain: PreparedPostShaderChain,
}

#[cfg(any(target_os = "macos", test))]
impl UserShaderRuntime {
    fn selected(&self, persisted_enabled: bool) -> bool {
        (persisted_enabled || self.suppress_shipped) && !self.locally_disabled
    }

    fn pause_for_unconfirmed_preferences(&mut self) -> bool {
        if self.authority_paused {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        self.authority_paused = true;
        self.active_fingerprint = None;
        self.attempted = None;
        true
    }

    fn explicitly_disable(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.locally_disabled = true;
        self.suppress_shipped = true;
        self.attempted = None;
        self.active_fingerprint = None;
    }

    fn explicitly_enable(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.locally_disabled = false;
        self.authority_paused = false;
        self.suppress_shipped = true;
        self.attempted = None;
    }

    fn completion_is_current(&self, generation: u64, revision: u64) -> bool {
        generation == self.generation && revision == self.debug_revision && !self.locally_disabled
    }

    fn should_start(&self, fingerprint: &str) -> bool {
        !self.locally_disabled
            && self.task.is_none()
            && self.attempted.as_ref().is_none_or(|(attempted, revision)| {
                attempted != fingerprint || *revision != self.debug_revision
            })
    }

    fn reconcile_unselected_source(&mut self, preparation_draining: bool) -> bool {
        let restore_shipped = self.suppress_shipped;
        self.suppress_shipped = false;
        self.active_fingerprint = None;
        self.attempted = None;
        if preparation_draining {
            self.pause_for_unconfirmed_preferences();
        } else {
            self.authority_paused = false;
        }
        restore_shipped
    }

    fn mark_failed(&mut self, status: String) {
        self.suppress_shipped = true;
        self.locally_disabled = true;
        self.active_fingerprint = None;
        self.owns_renderer = false;
        self.status = status;
    }
}

#[derive(Default)]
struct UserShaderImport {
    generation: u64,
    picker_open: bool,
    reading: bool,
}

impl UserShaderImport {
    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.picker_open = true;
        self.reading = false;
        self.generation
    }

    fn start_read(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) || !self.picker_open {
            return false;
        }
        self.picker_open = false;
        self.reading = true;
        true
    }

    fn finish(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.picker_open = false;
        self.reading = false;
        true
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.picker_open = false;
        self.reading = false;
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn busy(&self) -> bool {
        self.picker_open || self.reading
    }
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            updates: super::update_check::UpdateCheck::default(),
            payload: super::update_payload::UpdatePayload::default(),
            text_scale_percent: DEFAULT_TEXT_SCALE_PERCENT,
            rating_decay_weeks: DEFAULT_RATING_DECAY_WEEKS,
            trash_folder: None,
            previous_focus: None,
            picker_generation: 0,
            picking: false,
            status: String::new(),
            storage: None,
            maintenance_status: String::new(),
            maintenance: None,
            maintenance_task: None,
            shipped_visual_effect: ShippedVisualEffectPreferences::default(),
            visual_effect_local_enabled: None,
            visual_effect_last_attempt: None,
            visual_effect_debug_override: false,
            visual_effect_debug_revision: 0,
            visual_effect_status: String::new(),
            visual_effect_persistence_status: String::new(),
            user_shader_import: UserShaderImport::default(),
            user_shader_status: String::new(),
            reviewed_user_shader_fingerprint: None,
            #[cfg(target_os = "macos")]
            user_shader_runtime: UserShaderRuntime::default(),
        }
    }
}

enum PendingMaintenance {
    Usage(MaintenanceRequest<GlobalStorageUsage>),
    Clear(MaintenanceRequest<CacheClearOutcome>),
}

impl GeneralSettings {
    fn effective_visual_effect(&self) -> ShippedVisualEffectPreferences {
        let mut effect = self.shipped_visual_effect;
        if let Some(enabled) = self.visual_effect_local_enabled {
            effect.enabled = enabled;
        }
        effect
    }

    pub(super) fn mark_local_visual_effect_disable_unpersisted(&mut self, reason: &str) {
        // A late failure of an older disable request cannot describe a newer
        // explicit enable as locally disabled.
        if self.visual_effect_local_enabled != Some(false) {
            return;
        }
        self.visual_effect_persistence_status = format!(
            "Effect disabled for this session only; saved settings could not be updated: {reason}"
        );
    }

    #[cfg(target_os = "macos")]
    pub(super) fn picking(&self) -> bool {
        self.picking
    }

    #[cfg(target_os = "macos")]
    pub(super) fn maintenance_busy(&self) -> bool {
        self.maintenance.is_some()
    }

    pub(super) fn storage_label(&self) -> String {
        self.storage
            .map(|usage| {
                format!(
                    "{} total · Database {} · Cache {}",
                    format_bytes(usage.total_bytes()),
                    format_bytes(usage.database_bytes),
                    format_bytes(usage.cache_bytes)
                )
            })
            .unwrap_or_else(|| "Not measured".into())
    }

    #[cfg(target_os = "macos")]
    pub(super) fn status(&self) -> &str {
        &self.status
    }

    #[cfg(target_os = "macos")]
    pub(super) fn maintenance_status(&self) -> &str {
        &self.maintenance_status
    }

    pub(super) fn invalidate_picker(&mut self) {
        self.picker_generation = self.picker_generation.wrapping_add(1);
        self.picking = false;
        self.user_shader_import.invalidate();
        self.user_shader_status.clear();
    }

    fn begin_user_shader_import(&mut self) -> u64 {
        self.user_shader_status = "Choose a WGSL text file to import.".into();
        self.user_shader_import.begin()
    }

    fn start_user_shader_read(&mut self, generation: u64, name: &str) -> bool {
        if !self.user_shader_import.start_read(generation) {
            return false;
        }
        self.user_shader_status = format!("Reading {name}…");
        true
    }

    fn finish_user_shader_import(&mut self, generation: u64, status: String) -> bool {
        if !self.user_shader_import.finish(generation) {
            return false;
        }
        self.user_shader_status = status;
        true
    }

    fn cancel_user_shader_import(&mut self) {
        self.user_shader_import.invalidate();
        self.user_shader_status = "WGSL import cancelled; the saved source is unchanged.".into();
    }

    pub(super) fn user_shader_picker_open(&self) -> bool {
        self.user_shader_import.picker_open
    }

    pub(super) fn user_shader_import_busy(&self) -> bool {
        self.user_shader_import.busy()
    }

    #[cfg(target_os = "macos")]
    pub(super) fn user_shader_runtime_status(&self) -> &str {
        &self.user_shader_runtime.status
    }

    fn should_attempt_visual_effect(&mut self, effect: ShippedVisualEffectPreferences) -> bool {
        if self.visual_effect_last_attempt == Some(effect) {
            return false;
        }
        self.visual_effect_last_attempt = Some(effect);
        true
    }

    fn observe_visual_effect_debug_override(&mut self, enabled: bool, revision: u64) -> bool {
        if self.visual_effect_debug_override != enabled
            || self.visual_effect_debug_revision != revision
        {
            self.visual_effect_last_attempt = None;
            self.visual_effect_debug_override = enabled;
            self.visual_effect_debug_revision = revision;
            #[cfg(target_os = "macos")]
            {
                self.user_shader_runtime.generation =
                    self.user_shader_runtime.generation.wrapping_add(1);
                self.user_shader_runtime.debug_revision = revision;
                // Keep the task handle so its result can drain; generation and
                // revision checks make any late native result unusable.
                self.user_shader_runtime.locally_disabled = true;
                self.user_shader_runtime.attempted = None;
                self.user_shader_runtime.owns_renderer = false;
                self.user_shader_runtime.active_fingerprint = None;
                if self.user_shader_runtime.suppress_shipped {
                    self.user_shader_runtime.status =
                        "Custom shader paused after the visual-effects diagnostic changed; choose Enable again after review.".into();
                }
            }
            return true;
        }
        false
    }

    #[cfg(test)]
    fn local_visual_effect_disable_survives_owner_projection(&mut self) {
        self.visual_effect_local_enabled = Some(false);
        self.shipped_visual_effect.enabled = true;
    }
}

pub(super) fn visual_effect_preset_label(preset: ShippedVisualEffectPreset) -> &'static str {
    match preset {
        ShippedVisualEffectPreset::Grayscale => "Grayscale",
        ShippedVisualEffectPreset::Invert => "Invert",
        ShippedVisualEffectPreset::GrayscaleThenInvert => "Gray → Invert",
    }
}

fn shipped_visual_effect_shaders(effect: ShippedVisualEffectPreferences) -> Vec<String> {
    if !effect.enabled {
        return Vec::new();
    }
    let strength = f32::from(effect.strength_percent) / 100.0;
    let passes: &[&str] = match effect.preset {
        ShippedVisualEffectPreset::Grayscale => &["vec3<f32>(luma)"],
        ShippedVisualEffectPreset::Invert => &["vec3<f32>(color.a) - color.rgb"],
        ShippedVisualEffectPreset::GrayscaleThenInvert => {
            &["vec3<f32>(luma)", "vec3<f32>(color.a) - color.rgb"]
        }
    };
    passes
        .iter()
        .map(|processed| {
            format!(
                r#"
@group(0) @binding(0) var screen: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;

@fragment
fn post_fragment(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {{
    let color = textureSample(screen, screen_sampler, uv);
    let luma = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec4<f32>(mix(color.rgb, {processed}, {strength:.2}), color.a);
}}
"#
            )
        })
        .collect()
}

pub(super) fn visual_effect_strength_label(effect: ShippedVisualEffectPreferences) -> String {
    if effect.preset == ShippedVisualEffectPreset::GrayscaleThenInvert {
        format!("{}% per pass", effect.strength_percent)
    } else {
        format!("{}%", effect.strength_percent)
    }
}

impl Browser {
    pub(super) fn visual_effect_config(&self) -> ShippedVisualEffectPreferences {
        self.general_settings.effective_visual_effect()
    }

    fn set_visual_effect_enabled(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut effect = self.general_settings.shipped_visual_effect;
        effect.enabled = enabled;

        if enabled {
            // An explicit enable clears a prior emergency disable. If persistence is
            // unavailable, retain this explicit session choice instead of stale state.
            self.general_settings.visual_effect_local_enabled = Some(true);
        } else {
            // Latch disable before contacting the profile owner. Later snapshots cannot
            // resurrect a stale persisted `enabled: true` value.
            self.general_settings.visual_effect_local_enabled = Some(false);
        }

        let persisted = self.preferences_editable()
            && self.submit_preference_patch(PreferencePatch::SetShippedVisualEffect(effect), cx);
        if persisted {
            if enabled {
                self.general_settings.visual_effect_local_enabled = None;
            }
            self.apply_preference_projection(cx);
            self.general_settings
                .visual_effect_persistence_status
                .clear();
        } else if enabled {
            self.general_settings.visual_effect_persistence_status =
                "Effect enabled for this session only; settings could not be saved".into();
        } else {
            self.general_settings
                .mark_local_visual_effect_disable_unpersisted("settings unavailable");
        }
        self.apply_shipped_visual_effect(window);
        cx.notify();
    }

    pub(super) fn toggle_visual_effect(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_visual_effect_from_accessibility(window, cx);
    }

    pub(super) fn toggle_visual_effect_from_accessibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_visual_effect_enabled(!self.visual_effect_config().enabled, window, cx);
    }

    pub(super) fn cycle_visual_effect_preset(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let mut effect = self.general_settings.shipped_visual_effect;
        effect.enabled = self.visual_effect_config().enabled;
        effect.preset = match effect.preset {
            ShippedVisualEffectPreset::Grayscale => ShippedVisualEffectPreset::Invert,
            ShippedVisualEffectPreset::Invert => ShippedVisualEffectPreset::GrayscaleThenInvert,
            ShippedVisualEffectPreset::GrayscaleThenInvert => ShippedVisualEffectPreset::Grayscale,
        };
        if self.submit_preference_patch(PreferencePatch::SetShippedVisualEffect(effect), cx) {
            self.apply_preference_projection(cx);
            self.general_settings
                .visual_effect_persistence_status
                .clear();
            cx.notify();
        }
    }

    pub(super) fn change_visual_effect_strength(&mut self, delta: i16, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let mut effect = self.general_settings.shipped_visual_effect;
        effect.enabled = self.visual_effect_config().enabled;
        effect.strength_percent = (i32::from(effect.strength_percent) + i32::from(delta))
            .clamp(0, i32::from(MAX_VISUAL_EFFECT_STRENGTH_PERCENT))
            as u16;
        if self.submit_preference_patch(PreferencePatch::SetShippedVisualEffect(effect), cx) {
            self.apply_preference_projection(cx);
            self.general_settings
                .visual_effect_persistence_status
                .clear();
            cx.notify();
        }
    }

    pub(super) fn reconcile_shipped_visual_effect(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let diagnostic_override = cx
            .try_global::<crate::theme::DebugPostEffectOverride>()
            .copied()
            .unwrap_or_default();
        let debug_changed = self.general_settings.observe_visual_effect_debug_override(
            diagnostic_override.enabled,
            diagnostic_override.revision,
        );
        if diagnostic_override.enabled {
            return;
        }
        if debug_changed {
            let _ = window.set_post_shader(None);
        }
        if self.reconcile_user_shader(window, cx) {
            return;
        }
        self.apply_shipped_visual_effect(window);
    }

    #[cfg(target_os = "macos")]
    fn reconcile_user_shader(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let confirmed = self.preferences.confirmed_snapshot().cloned();
        let Some(confirmed) = confirmed else {
            let preference_status = self.preference_status().to_owned();
            let diagnostic_override = self.general_settings.visual_effect_debug_override;
            let runtime = &mut self.general_settings.user_shader_runtime;
            if runtime.suppress_shipped || runtime.owns_renderer || runtime.task.is_some() {
                let changed = !runtime.authority_paused || runtime.owns_renderer;
                if !runtime.authority_paused {
                    runtime.pause_for_unconfirmed_preferences();
                }
                if runtime.owns_renderer && !diagnostic_override {
                    let _ = window.set_post_shader(None);
                    runtime.owns_renderer = false;
                }
                runtime.suppress_shipped = true;
                if changed {
                    runtime.status = format!(
                        "Custom shader paused because settings are not confirmed. {}",
                        preference_status
                    );
                    cx.notify();
                }
                return true;
            }
            return false;
        };

        let Some(source) = confirmed.enabled_user_shader_source() else {
            let runtime = &mut self.general_settings.user_shader_runtime;
            let restore_shipped = runtime.reconcile_unselected_source(runtime.task.is_some());
            if runtime.owns_renderer {
                let _ = window.set_post_shader(None);
                runtime.owns_renderer = false;
            }
            runtime.status.clear();
            if restore_shipped {
                self.general_settings.visual_effect_last_attempt = None;
            }
            return false;
        };

        let fingerprint = match user_shader_content_fingerprint(source) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let runtime = &mut self.general_settings.user_shader_runtime;
                runtime.suppress_shipped = true;
                runtime.mark_failed(format!(
                    "Custom shader is unavailable: {error}; showing normal UI. Choose Enable to retry."
                ));
                return true;
            }
        };
        let runtime = &mut self.general_settings.user_shader_runtime;
        runtime.suppress_shipped = true;
        if runtime.task.is_some()
            && runtime
                .attempted
                .as_ref()
                .is_some_and(|(attempted, _)| attempted != &fingerprint)
        {
            if !runtime.authority_paused {
                runtime.pause_for_unconfirmed_preferences();
            }
            runtime.active_fingerprint = None;
            if runtime.owns_renderer {
                let _ = window.set_post_shader(None);
                runtime.owns_renderer = false;
            }
            runtime.status =
                "Custom shader source changed; waiting for the earlier preparation to finish."
                    .into();
            return true;
        }
        runtime.authority_paused = false;
        if runtime.locally_disabled {
            if runtime.owns_renderer {
                let _ = window.set_post_shader(None);
                runtime.owns_renderer = false;
                runtime.active_fingerprint = None;
            }
            return true;
        }
        if runtime.active_fingerprint.as_deref() == Some(&fingerprint) {
            return true;
        }
        if !runtime.should_start(&fingerprint) {
            return true;
        }

        runtime.generation = runtime.generation.wrapping_add(1);
        let generation = runtime.generation;
        let debug_revision = runtime.debug_revision;
        runtime.attempted = Some((fingerprint.clone(), debug_revision));
        runtime.status = "Preparing the approved custom shader…".into();
        if runtime.owns_renderer {
            let _ = window.set_post_shader(None);
            runtime.owns_renderer = false;
            runtime.active_fingerprint = None;
        } else {
            // Custom shader selection is exclusive with the shipped visual effects.
            let _ = window.set_post_shader(None);
        }

        let compiler = match window.post_shader_compiler() {
            Ok(compiler) => compiler,
            Err(error) => {
                runtime.mark_failed(format!(
                    "Custom shader unavailable: {error}; showing normal UI. Choose Enable to retry."
                ));
                cx.notify();
                return true;
            }
        };
        let source = source.to_owned();
        let fingerprint_for_task = fingerprint.clone();
        let Some(permit) = PreparationPermit::try_acquire() else {
            runtime.mark_failed(
                "Another custom shader is still preparing; choose Enable again to retry.".into(),
            );
            cx.notify();
            return true;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _permit = permit;
                    validate_user_shader_abi(&source)?;
                    let chain = compiler
                        .prepare_chain(&[source])
                        .map_err(|error| error.to_string())?;
                    Ok(PreparedUserShader {
                        fingerprint: fingerprint_for_task,
                        chain,
                    })
                })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.finish_user_shader_preparation(generation, debug_revision, result, window, cx);
            });
        });
        self.general_settings.user_shader_runtime.task = Some(task);
        cx.notify();
        true
    }

    #[cfg(target_os = "macos")]
    fn finish_user_shader_preparation(
        &mut self,
        generation: u64,
        debug_revision: u64,
        result: Result<PreparedUserShader, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let diagnostic_override = self.general_settings.visual_effect_debug_override;
        let diagnostic_state = cx
            .try_global::<crate::theme::DebugPostEffectOverride>()
            .copied()
            .unwrap_or_default();
        let runtime = &mut self.general_settings.user_shader_runtime;
        runtime.task.take();
        if !runtime.completion_is_current(generation, debug_revision) {
            // The settled desired source may have been waiting for this stale
            // request to drain before it can start its own bounded preparation.
            cx.notify();
            return;
        }
        let confirmed_source = self
            .preferences
            .confirmed_snapshot()
            .and_then(UiPreferences::enabled_user_shader_source);
        let fingerprint_matches = confirmed_source
            .and_then(|source| user_shader_content_fingerprint(source).ok())
            .as_deref()
            == Some(
                runtime
                    .attempted
                    .as_ref()
                    .map(|(fingerprint, _)| fingerprint.as_str())
                    .unwrap_or_default(),
            );
        if !fingerprint_matches
            || diagnostic_override
            || diagnostic_state.enabled
            || diagnostic_state.revision != debug_revision
        {
            runtime.generation = runtime.generation.wrapping_add(1);
            runtime.mark_failed(
                "Custom shader result expired; choose Enable again to prepare the current saved source.".into(),
            );
            cx.notify();
            return;
        }
        match result {
            Ok(prepared) if prepared.fingerprint == runtime.attempted.as_ref().unwrap().0 => {
                match window.install_prepared_post_shader_chain(prepared.chain) {
                    Ok(()) => {
                        runtime.active_fingerprint = Some(prepared.fingerprint);
                        runtime.owns_renderer = true;
                        runtime.status = "Approved custom shader active.".into();
                    }
                    Err(error) => {
                        runtime.mark_failed(format!(
                            "Custom shader could not be installed: {error}; showing normal UI. Choose Enable to retry."
                        ));
                    }
                }
            }
            Ok(_) => {
                runtime.mark_failed(
                    "Custom shader result expired; choose Enable again to prepare the current saved source.".into(),
                );
            }
            Err(error) => {
                runtime.mark_failed(format!(
                    "Custom shader unavailable: {error}; showing normal UI. Choose Enable to retry."
                ));
            }
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn disable_user_shader_locally(&mut self, window: &mut Window, reason: &str) {
        let runtime = &mut self.general_settings.user_shader_runtime;
        runtime.explicitly_disable();
        runtime.status = reason.to_owned();
        if !self.general_settings.visual_effect_debug_override && runtime.owns_renderer {
            let _ = window.set_post_shader(None);
        }
        runtime.owns_renderer = false;
    }

    #[cfg(not(target_os = "macos"))]
    fn reconcile_user_shader(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        false
    }

    fn apply_shipped_visual_effect(&mut self, window: &mut Window) {
        #[cfg(target_os = "macos")]
        if self.general_settings.user_shader_runtime.suppress_shipped {
            return;
        }
        let effect = self.visual_effect_config();
        if !self.general_settings.should_attempt_visual_effect(effect) {
            return;
        }
        let sources = shipped_visual_effect_shaders(effect);
        let result = if sources.len() <= 1 {
            // Preserve the legacy single-pass and clear routes on every backend.
            window.set_post_shader(sources.first().map(String::as_str))
        } else {
            let source_refs: Vec<&str> = sources.iter().map(String::as_str).collect();
            window
                .set_post_shaders(&source_refs)
                .map_err(|error| error.to_string())
        };
        match result {
            Ok(()) if effect.enabled => {
                self.general_settings.visual_effect_status = format!(
                    "{} effect active at {}",
                    visual_effect_preset_label(effect.preset),
                    visual_effect_strength_label(effect)
                );
            }
            Ok(()) => self.general_settings.visual_effect_status = "Visual effects off".into(),
            Err(error) => {
                self.general_settings.visual_effect_status =
                    format!("Visual effect unavailable: {error}; showing normal UI");
            }
        }
    }

    pub(super) fn toggle_text_scale(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_text_scale_from_accessibility(cx);
    }

    pub(super) fn toggle_text_scale_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let percent = if self.general_settings.text_scale_percent == DEFAULT_TEXT_SCALE_PERCENT {
            ENLARGED_TEXT_SCALE_PERCENT
        } else {
            DEFAULT_TEXT_SCALE_PERCENT
        };
        if self.submit_preference_patch(PreferencePatch::SetTextScalePercent(percent), cx) {
            self.apply_preference_projection(cx);
            cx.notify();
        }
    }

    pub(super) fn close_general_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_settings_open = false;
        self.general_settings.updates.close();
        self.general_settings.payload.close();
        self.general_settings.invalidate_picker();
        let restore = self
            .general_settings
            .previous_focus
            .take()
            .unwrap_or_else(|| {
                self.ax_focus("settings-open")
                    .unwrap_or_else(|| self.focus_handle.clone())
            });
        window.focus(&restore);
        cx.notify();
    }

    pub(super) fn change_rating_decay(&mut self, delta: i16, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let weeks = (i32::from(self.general_settings.rating_decay_weeks) + i32::from(delta)).clamp(
            i32::from(MIN_RATING_DECAY_WEEKS),
            i32::from(MAX_RATING_DECAY_WEEKS),
        ) as u16;
        if self.submit_preference_patch(PreferencePatch::SetRatingDecayWeeks(weeks), cx) {
            self.general_settings.rating_decay_weeks = weeks;
            cx.notify();
        }
    }

    pub(super) fn toggle_anti_click(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let (enabled, fade_ms) = self.preferences.anti_click_settings();
        if self.submit_preference_patch(
            PreferencePatch::SetAntiClickSettings {
                enabled: !enabled,
                fade_ms,
            },
            cx,
        ) {
            cx.notify();
        }
    }

    pub(super) fn change_anti_click_fade(&mut self, delta_ms: f32, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let (enabled, fade_ms) = self.preferences.anti_click_settings();
        let next_fade_ms = (fade_ms + delta_ms).clamp(0.0, MAX_ANTI_CLICK_FADE_MS);
        if next_fade_ms.to_bits() == fade_ms.to_bits() {
            return;
        }
        if self.submit_preference_patch(
            PreferencePatch::SetAntiClickSettings {
                enabled,
                fade_ms: next_fade_ms,
            },
            cx,
        ) {
            cx.notify();
        }
    }

    fn choose_user_shader(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_user_shader_from_accessibility(window, cx);
    }

    pub(super) fn choose_user_shader_from_accessibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences.ready_for_maintenance() || !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        if self.general_settings.user_shader_picker_open() {
            return;
        }
        self.disable_user_shader_locally(
            window,
            "Custom shader paused while choosing a replacement source.",
        );
        let generation = self.general_settings.begin_user_shader_import();
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a WGSL source text file".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = picker.await;
            let path = match picked {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.general_settings.finish_user_shader_import(
                            generation,
                            "WGSL selection cancelled; the saved source is unchanged.".into(),
                        ) {
                            cx.notify();
                        }
                    });
                    return;
                }
                Ok(Err(error)) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.general_settings.finish_user_shader_import(
                            generation,
                            format!("WGSL file selection failed: {error}"),
                        ) {
                            cx.notify();
                        }
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.general_settings.finish_user_shader_import(
                            generation,
                            format!("WGSL file selection unavailable: {error}"),
                        ) {
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let Some(path) = path else {
                let _ = this.update(cx, |view, cx| {
                    if view.general_settings.finish_user_shader_import(
                        generation,
                        "No file selected; the saved source is unchanged.".into(),
                    ) {
                        cx.notify();
                    }
                });
                return;
            };

            let display_name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| "selected file".into())
                .chars()
                .take(80)
                .collect::<String>();
            let started = this
                .update(cx, |view, _| {
                    view.general_settings
                        .start_user_shader_read(generation, &display_name)
                })
                .unwrap_or(false);
            if !started {
                return;
            }

            let read_path = path;
            let result = cx
                .background_executor()
                .spawn(async move { read_user_shader_file(&read_path) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.general_settings.user_shader_import.is_current(generation) {
                    return;
                }
                view.general_settings
                    .finish_user_shader_import(generation, String::new());
                match result {
                    Ok(source) => {
                        if !view.preferences.ready_for_maintenance()
                            || !view.preferences_editable()
                        {
                            view.general_settings.user_shader_status = format!(
                                "Could not import {display_name}: settings changed while the file was being read; the saved source is unchanged."
                            );
                        } else {
                            match user_shader_content_fingerprint(&source) {
                                Ok(_) => {
                                    if view.submit_preference_patch(
                                        PreferencePatch::ImportUserShader { source },
                                        cx,
                                    ) {
                                        // The effective projection may now show this exact text,
                                        // but review and approval wait for the durable owner ack.
                                        view.general_settings.user_shader_status.clear();
                                    } else {
                                        view.general_settings.user_shader_status = format!(
                                            "Could not import {display_name}: {}",
                                            view.preference_status()
                                        );
                                    }
                                }
                                Err(error) => {
                                    view.general_settings.user_shader_status = format!(
                                        "Could not import {display_name}: {error}; the saved source is unchanged."
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        view.general_settings.user_shader_status = format!(
                            "Could not import {display_name}: {error}; the saved source is unchanged."
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn cancel_user_shader_import(&mut self, cx: &mut Context<Self>) {
        if self.general_settings.user_shader_import_busy() {
            self.general_settings.cancel_user_shader_import();
            cx.notify();
        }
    }

    pub(super) fn approve_reviewed_user_shader(
        &mut self,
        expected_fingerprint: String,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences.ready_for_maintenance() || !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        if self.general_settings.user_shader_import_busy() {
            return;
        }
        let Some(confirmed) = self.preferences.confirmed_snapshot() else {
            return;
        };
        if !confirmed_shader_is_reviewed(confirmed, &expected_fingerprint) {
            self.general_settings.user_shader_status =
                "The displayed source changed; review the current text before approving it.".into();
            cx.notify();
            return;
        }
        let Some(source) = confirmed.user_shader.as_ref() else {
            return;
        };
        if source.approved_fingerprint.as_deref() == Some(&expected_fingerprint) {
            return;
        }
        self.general_settings.user_shader_status.clear();
        if self.submit_preference_patch(
            PreferencePatch::ApproveUserShader {
                expected_fingerprint,
            },
            cx,
        ) {
            self.apply_preference_projection(cx);
        } else {
            self.general_settings.user_shader_status =
                format!("Approval was not submitted: {}", self.preference_status());
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn approve_user_shader_from_accessibility(&mut self, cx: &mut Context<Self>) {
        let Some(fingerprint) = self
            .general_settings
            .reviewed_user_shader_fingerprint
            .clone()
        else {
            return;
        };
        self.approve_reviewed_user_shader(fingerprint, cx);
    }

    pub(super) fn refresh_user_shader_review_token(&mut self) {
        self.general_settings.reviewed_user_shader_fingerprint = self
            .preferences
            .effective()
            .user_shader
            .as_ref()
            .and_then(|shader| user_shader_content_fingerprint(&shader.source).ok());
    }

    pub(super) fn revoke_user_shader_approval(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.disable_user_shader_locally(
            window,
            "Custom shader disabled while approval is revoked.",
        );
        if !self.preferences.ready_for_maintenance() || !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        self.general_settings.cancel_user_shader_import();
        let has_source = self
            .preferences
            .confirmed_snapshot()
            .is_some_and(|preferences| preferences.user_shader.is_some());
        if has_source && self.submit_preference_patch(PreferencePatch::RevokeUserShaderApproval, cx)
        {
            self.general_settings.user_shader_status.clear();
            self.apply_preference_projection(cx);
        }
        cx.notify();
    }

    pub(super) fn clear_user_shader(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.disable_user_shader_locally(
            window,
            "Custom shader disabled while its source is cleared.",
        );
        if !self.preferences.ready_for_maintenance() || !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        self.general_settings.cancel_user_shader_import();
        let has_source = self
            .preferences
            .confirmed_snapshot()
            .is_some_and(|preferences| preferences.user_shader.is_some());
        if has_source && self.submit_preference_patch(PreferencePatch::ClearUserShader, cx) {
            self.general_settings.user_shader_status.clear();
            self.apply_preference_projection(cx);
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn set_user_shader_enabled(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !enabled {
            self.disable_user_shader_locally(window, "Custom shader disabled for this window.");
            let has_source = self.preferences.effective().user_shader.is_some();
            if has_source
                && self.preferences.ready_for_maintenance()
                && self.preferences_editable()
                && self.submit_preference_patch(PreferencePatch::SetUserShaderEnabled(false), cx)
            {
                self.apply_preference_projection(cx);
            } else if has_source {
                self.general_settings.user_shader_runtime.status =
                    "Custom shader is disabled for this window; the saved setting could not be updated yet.".into();
            }
            cx.notify();
            return;
        }

        let Some(confirmed) = self.preferences.confirmed_snapshot() else {
            self.general_settings.user_shader_runtime.status = format!(
                "Custom shader cannot be enabled until settings are confirmed. {}",
                self.preference_status()
            );
            cx.notify();
            return;
        };
        let approved = confirmed.user_shader.as_ref().is_some_and(|shader| {
            user_shader_content_fingerprint(&shader.source)
                .ok()
                .is_some_and(|fingerprint| {
                    shader.approved_fingerprint.as_deref() == Some(fingerprint.as_str())
                })
        });
        if !approved || !self.preferences_editable() {
            self.general_settings.user_shader_runtime.status =
                "Approve the exact saved source before enabling it.".into();
            cx.notify();
            return;
        }
        if self.general_settings.visual_effect_debug_override {
            self.general_settings.user_shader_runtime.status =
                "Custom shader cannot be enabled while the visual-effects diagnostic is active."
                    .into();
            cx.notify();
            return;
        }
        self.general_settings
            .user_shader_runtime
            .explicitly_enable();
        self.general_settings.user_shader_runtime.status =
            "Saving the custom shader selection…".into();
        if self.submit_preference_patch(PreferencePatch::SetUserShaderEnabled(true), cx) {
            self.apply_preference_projection(cx);
        } else {
            self.general_settings.user_shader_runtime.locally_disabled = true;
            self.general_settings.user_shader_runtime.status = format!(
                "Custom shader was not enabled: {}",
                self.preference_status()
            );
        }
        cx.notify();
    }

    /// One selection predicate for visible controls, accessibility, and toggle actions.
    /// A pending save may remove confirmed authority but must still allow local disable.
    pub(super) fn user_shader_selected_for_controls(&self) -> bool {
        let persisted_enabled = self
            .preferences
            .confirmed_snapshot()
            .and_then(|preferences| preferences.user_shader.as_ref())
            .is_some_and(|shader| shader.enabled);
        #[cfg(target_os = "macos")]
        {
            self.general_settings
                .user_shader_runtime
                .selected(persisted_enabled)
        }
        #[cfg(not(target_os = "macos"))]
        {
            persisted_enabled
        }
    }

    #[cfg(target_os = "macos")]
    fn set_user_shader_enabled_click(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_user_shader_from_accessibility(window, cx);
    }

    #[cfg(target_os = "macos")]
    pub(super) fn toggle_user_shader_from_accessibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let enabled = self.user_shader_selected_for_controls();
        self.set_user_shader_enabled(!enabled, window, cx);
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn set_user_shader_enabled(
        &mut self,
        enabled: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preferences.ready_for_maintenance()
            && self.preferences_editable()
            && self.submit_preference_patch(PreferencePatch::SetUserShaderEnabled(enabled), cx)
        {
            self.apply_preference_projection(cx);
        }
        cx.notify();
    }

    #[cfg(not(target_os = "macos"))]
    fn set_user_shader_enabled_click(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_user_shader_from_accessibility(window, cx);
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn toggle_user_shader_from_accessibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let enabled = self
            .preferences
            .confirmed_snapshot()
            .and_then(|preferences| preferences.user_shader.as_ref())
            .is_some_and(|shader| shader.enabled);
        self.set_user_shader_enabled(!enabled, window, cx);
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn disable_user_shader_locally(&mut self, _window: &mut Window, _reason: &str) {}

    pub(super) fn copy_user_shader_source(&mut self, cx: &mut Context<Self>) {
        let source = self
            .preferences
            .effective()
            .user_shader
            .map(|shader| shader.source);
        if let Some(source) = source {
            cx.write_to_clipboard(ClipboardItem::new_string(source));
            self.general_settings.user_shader_status =
                "Copied the exact source text to the clipboard.".into();
            cx.notify();
        }
    }

    pub(super) fn copy_user_shader_source_text(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_user_shader_source(cx);
    }

    fn choose_trash_folder(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.choose_trash_folder_from_accessibility(cx);
    }

    pub(super) fn choose_trash_folder_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable()
            || self.general_settings.picking
            || self.general_settings.maintenance.is_some()
        {
            return;
        }
        self.general_settings.picker_generation =
            self.general_settings.picker_generation.wrapping_add(1);
        let generation = self.general_settings.picker_generation;
        self.general_settings.picking = true;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a trash folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picker.await;
            let _ = this.update(cx, |view, cx| {
                if generation != view.general_settings.picker_generation {
                    return;
                }
                view.general_settings.picking = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next()
                            && view.preferences_editable()
                            && view.general_settings.maintenance.is_none()
                            && view.submit_preference_patch(
                                PreferencePatch::SetTrashFolder(Some(path.clone())),
                                cx,
                            )
                        {
                            view.general_settings.trash_folder = Some(path);
                            view.general_settings.status.clear();
                        }
                    }
                    Ok(Err(error)) => {
                        view.general_settings.status = format!("Folder selection failed: {error}")
                    }
                    Err(error) => {
                        view.general_settings.status =
                            format!("Folder selection unavailable: {error}")
                    }
                    Ok(Ok(None)) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn clear_trash_folder(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_trash_folder_from_accessibility(cx);
    }

    pub(super) fn clear_trash_folder_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() || self.general_settings.maintenance.is_some() {
            return;
        }
        if self.submit_preference_patch(PreferencePatch::SetTrashFolder(None), cx) {
            self.general_settings.picker_generation =
                self.general_settings.picker_generation.wrapping_add(1);
            self.general_settings.picking = false;
            self.general_settings.trash_folder = None;
            self.general_settings.status.clear();
            cx.notify();
        }
    }

    pub(super) fn refresh_global_storage(&mut self, cx: &mut Context<Self>) {
        if self.general_settings.maintenance.is_some() {
            return;
        }
        let request = cx
            .try_global::<SourceRegistryGlobal>()
            .ok_or(yield_session::SubmitError::Closed)
            .and_then(SourceRegistryGlobal::request_storage_usage);
        match request {
            Ok(request) => {
                self.general_settings.storage = None;
                self.general_settings.maintenance = Some(PendingMaintenance::Usage(request));
                self.general_settings.maintenance_status = "Measuring storage…".into();
                self.poll_general_maintenance(cx);
            }
            Err(error) => {
                self.general_settings.maintenance_status =
                    format!("Storage query unavailable: {error}")
            }
        }
        cx.notify();
    }

    pub(super) fn clear_rebuildable_cache(&mut self, cx: &mut Context<Self>) {
        if self.general_settings.maintenance.is_some()
            || !self.preferences.loaded()
            || !self.preferences_editable()
            || self.preferences.has_pending()
        {
            return;
        }
        let request = cx
            .try_global::<SourceRegistryGlobal>()
            .ok_or(yield_session::SubmitError::Closed)
            .and_then(SourceRegistryGlobal::clear_rebuildable_cache);
        match request {
            Ok(request) => {
                self.general_settings.invalidate_picker();
                // Admission can change cache contents before a completion is received.
                // Invalidate the old measurement even if the service later disconnects.
                self.general_settings.storage = None;
                self.general_settings.maintenance = Some(PendingMaintenance::Clear(request));
                self.general_settings.maintenance_status = "Clearing rebuildable caches…".into();
                self.poll_general_maintenance(cx);
            }
            Err(error) => {
                self.general_settings.maintenance_status =
                    format!("Cache maintenance unavailable: {error}")
            }
        }
        cx.notify();
    }

    fn poll_general_maintenance(&mut self, cx: &mut Context<Self>) {
        self.general_settings.maintenance_task = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(100)).await;
                let pending = this.update(cx, |view, cx| view.drain_general_maintenance(cx));
                if !matches!(pending, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn drain_general_maintenance(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(request) = self.general_settings.maintenance.as_mut() else {
            return false;
        };
        let result = match request {
            PendingMaintenance::Usage(request) => match request.try_result() {
                Ok(Ok(usage)) => {
                    self.general_settings.storage = Some(usage);
                    Some(Ok("Storage usage updated".into()))
                }
                Ok(Err(error)) => Some(Err(error.to_string())),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("Storage service disconnected".into())),
            },
            PendingMaintenance::Clear(request) => match request.try_result() {
                Ok(Ok(outcome)) => {
                    // Old cache bytes are no longer authoritative after any cleanup.
                    self.general_settings.storage = None;
                    Some(if outcome.incomplete || outcome.namespace_replaced {
                        Err(format!(
                            "Cache cleanup incomplete: {} files removed; {}",
                            outcome.removed_files,
                            if outcome.remaining_entries_known {
                                format!("{} entries remain", outcome.remaining_entries)
                            } else {
                                "remaining entries were not measured".into()
                            }
                        ))
                    } else {
                        Ok(format!(
                            "Caches cleared: {} files, {} removed. Refresh to measure storage.",
                            outcome.removed_files,
                            format_bytes(outcome.removed_bytes)
                        ))
                    })
                }
                Ok(Err(error)) => {
                    self.general_settings.storage = None;
                    Some(Err(error.to_string()))
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Cache service disconnected; cleanup outcome unknown".into(),
                )),
            },
        };
        let Some(result) = result else {
            return true;
        };
        self.general_settings.maintenance.take();
        self.general_settings.maintenance_status = match result {
            Ok(message) => message,
            Err(message) => format!("Maintenance failed: {message}"),
        };
        cx.notify();
        false
    }

    pub(super) fn render_general_settings(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let effect = self.visual_effect_config();
        let diagnostic_override = cx
            .try_global::<crate::theme::DebugPostEffectOverride>()
            .is_some_and(|state| state.enabled);
        let effect_controls_enabled = self.preferences_editable() && !diagnostic_override;
        let editable = self.preferences_editable();
        let user_shader_preferences = self.preferences.effective();
        let user_shader_preview = user_shader_preferences.user_shader.as_ref();
        let user_shader_fingerprint = user_shader_preview
            .and_then(|shader| user_shader_content_fingerprint(&shader.source).ok());
        let user_shader_status = user_shader_review_status(
            self.preferences.confirmed_snapshot(),
            user_shader_preview,
            self.preferences.has_pending_user_shader_patch(),
            self.general_settings.user_shader_import_busy(),
            &self.general_settings.user_shader_status,
            self.preference_status(),
        );
        let user_shader_ready = editable
            && self.preferences.ready_for_maintenance()
            && !self.general_settings.user_shader_import_busy();
        let confirmed_shader = self
            .preferences
            .confirmed_snapshot()
            .and_then(|preferences| preferences.user_shader.as_ref());
        let shader_approval_matches = confirmed_shader.is_some_and(|shader| {
            user_shader_content_fingerprint(&shader.source)
                .ok()
                .is_some_and(|fingerprint| {
                    shader.approved_fingerprint.as_deref() == Some(fingerprint.as_str())
                })
        });
        let user_shader_selected = self.user_shader_selected_for_controls();
        let user_shader_toggle_enabled = user_shader_preview.is_some()
            && (user_shader_selected
                || (cfg!(target_os = "macos") && user_shader_ready && shader_approval_matches))
            && !diagnostic_override;
        #[cfg(target_os = "macos")]
        let user_shader_status = format!(
            "{user_shader_status} {}",
            self.general_settings.user_shader_runtime.status
        );
        let maintenance_busy = self.general_settings.maintenance.is_some();
        let trash_editable = editable && !maintenance_busy;
        let choose_enabled = trash_editable && !self.general_settings.picking;
        let clear_enabled = !maintenance_busy
            && self.preferences.loaded()
            && editable
            && !self.preferences.has_pending();
        let decrease_enabled =
            editable && self.general_settings.rating_decay_weeks > MIN_RATING_DECAY_WEEKS;
        let increase_enabled =
            editable && self.general_settings.rating_decay_weeks < MAX_RATING_DECAY_WEEKS;
        let (anti_click_enabled, anti_click_fade_ms) = self.preferences.anti_click_settings();
        let anti_click_decrease_enabled = editable && anti_click_fade_ms > 0.0;
        let anti_click_increase_enabled = editable && anti_click_fade_ms < MAX_ANTI_CLICK_FADE_MS;
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .min_w(px(0.0))
            .gap(px(theme.spacing.control_gap))
            .p(px(theme.spacing.panel_padding))
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child("GENERAL")
                    .child(div().flex_1())
                    .child(
                        control(self, "general-settings-close", "Close", true, &theme).on_click(
                            cx.listener(|view, _, window, cx| {
                                view.close_general_settings(window, cx)
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Anti-click fades"))
                    .child(
                        control(
                            self,
                            "anti-click-toggle",
                            if anti_click_enabled { "On" } else { "Off" },
                            editable,
                            &theme,
                        )
                        .when(editable, |button| {
                            button.on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_anti_click(cx)
                            }))
                        }),
                    )
                    .child(
                        control(
                            self,
                            "anti-click-fade-decrease",
                            "−",
                            anti_click_decrease_enabled,
                            &theme,
                        )
                        .when(anti_click_decrease_enabled, |button| {
                            button.on_click(cx.listener(|view, _, _, cx| {
                                view.change_anti_click_fade(-1.0, cx)
                            }))
                        }),
                    )
                    .child(format!("{anti_click_fade_ms} ms"))
                    .child(
                        control(
                            self,
                            "anti-click-fade-increase",
                            "+",
                            anti_click_increase_enabled,
                            &theme,
                        )
                        .when(anti_click_increase_enabled, |button| {
                            button.on_click(cx.listener(|view, _, _, cx| {
                                view.change_anti_click_fade(1.0, cx)
                            }))
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Rating Decay"))
                    .child(
                        control(self, "rating-decay-decrease", "−", decrease_enabled, &theme).when(
                            decrease_enabled,
                            |button| {
                                button.on_click(
                                    cx.listener(|view, _, _, cx| view.change_rating_decay(-1, cx)),
                                )
                            },
                        ),
                    )
                    .child(format!(
                        "{} weeks",
                        self.general_settings.rating_decay_weeks
                    ))
                    .child(
                        control(self, "rating-decay-increase", "+", increase_enabled, &theme).when(
                            increase_enabled,
                            |button| {
                                button.on_click(
                                    cx.listener(|view, _, _, cx| view.change_rating_decay(1, cx)),
                                )
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Visual Effect"))
                    .child(
                        control(
                            self,
                            "visual-effect-preset",
                            visual_effect_preset_label(effect.preset),
                            effect_controls_enabled,
                            &theme,
                        )
                        .when(effect_controls_enabled, |button| {
                            button.on_click(
                                cx.listener(|view, _, _, cx| view.cycle_visual_effect_preset(cx)),
                            )
                        }),
                    )
                    .child(
                        control(
                            self,
                            "visual-effect-toggle",
                            if effect.enabled { "On" } else { "Off" },
                            !diagnostic_override,
                            &theme,
                        )
                        .when(!diagnostic_override, |button| {
                            button.on_click(cx.listener(Self::toggle_visual_effect))
                        }),
                    )
                    .child(visual_effect_strength_label(effect))
                    .child(
                        control(
                            self,
                            "visual-effect-strength-decrease",
                            "−",
                            effect_controls_enabled && effect.strength_percent > 0,
                            &theme,
                        )
                        .when(
                            effect_controls_enabled && effect.strength_percent > 0,
                            |button| {
                                button.on_click(cx.listener(|view, _, _, cx| {
                                    view.change_visual_effect_strength(-10, cx)
                                }))
                            },
                        ),
                    )
                    .child(
                        control(
                            self,
                            "visual-effect-strength-increase",
                            "+",
                            effect_controls_enabled
                                && effect.strength_percent < MAX_VISUAL_EFFECT_STRENGTH_PERCENT,
                            &theme,
                        )
                        .when(
                            effect_controls_enabled
                                && effect.strength_percent < MAX_VISUAL_EFFECT_STRENGTH_PERCENT,
                            |button| {
                                button.on_click(cx.listener(|view, _, _, cx| {
                                    view.change_visual_effect_strength(10, cx)
                                }))
                            },
                        ),
                    ),
            )
            .when(diagnostic_override, |panel| {
                panel.child(div().child("Shipped effects paused by the debug post effect override"))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Trash Folder"))
                    .child(
                        div().flex_1().min_w(px(0.0)).overflow_hidden().child(
                            self.general_settings
                                .trash_folder
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "No trash folder configured".into()),
                        ),
                    )
                    .child(
                        control(
                            self,
                            "choose-trash-folder",
                            if self.general_settings.picking {
                                "Choosing…"
                            } else {
                                "Choose Folder"
                            },
                            choose_enabled,
                            &theme,
                        )
                        .when(choose_enabled, |button| {
                            button.on_click(cx.listener(Self::choose_trash_folder))
                        }),
                    )
                    .child(
                        control(self, "clear-trash-folder", "Clear", trash_editable, &theme)
                            .when(trash_editable, |button| {
                                button.on_click(cx.listener(Self::clear_trash_folder))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Custom WGSL"))
                    .child(
                        control(
                            self,
                            "user-shader-import",
                            if self.general_settings.user_shader_import_busy() {
                                "Choose Another Source…"
                            } else {
                                "Import Source…"
                            },
                            editable
                                && self.preferences.ready_for_maintenance()
                                && !self.general_settings.user_shader_picker_open(),
                            &theme,
                        )
                        .when(
                            editable
                                && self.preferences.ready_for_maintenance()
                                && !self.general_settings.user_shader_picker_open(),
                            |button| {
                                button.on_click(cx.listener(Self::choose_user_shader))
                            },
                        ),
                    )
                    .when(self.general_settings.user_shader_import_busy(), |row| {
                        row.child(
                            control(self, "user-shader-cancel", "Cancel Import", true, &theme)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.cancel_user_shader_import(cx)
                                })),
                        )
                    })
                    .child({
                        let enabled = user_shader_ready
                            && user_shader_fingerprint.is_some()
                            && !shader_approval_matches;
                        let reviewed = user_shader_fingerprint.clone();
                        control(
                            self,
                            "user-shader-approve",
                            if shader_approval_matches {
                                "Approved"
                            } else {
                                "Approve Reviewed Source"
                            },
                            enabled,
                            &theme,
                        )
                        .when(enabled, |button| {
                            button.on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(fingerprint) = reviewed.clone() {
                                    view.approve_reviewed_user_shader(fingerprint, cx);
                                }
                            }))
                        })
                    })
                    .when(
                        user_shader_preview.is_some_and(|shader| {
                            shader.approved_fingerprint.is_some()
                        }),
                        |row| {
                            row.child(
                                control(
                                    self,
                                    "user-shader-revoke",
                                    "Revoke Approval",
                                    true,
                                    &theme,
                                )
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.revoke_user_shader_approval(window, cx)
                                })),
                            )
                        },
                    )
                    .when(user_shader_preview.is_some(), |row| {
                        row.child(
                            control(
                                self,
                                "user-shader-clear",
                                "Clear Source",
                                true,
                                &theme,
                            )
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.clear_user_shader(window, cx)
                            })),
                        )
                    })
                    .when(user_shader_preview.is_some(), |row| {
                        row.child(
                            control(
                                self,
                                "user-shader-toggle",
                                if user_shader_selected {
                                    "Disable Custom Shader"
                                } else {
                                    "Enable Custom Shader"
                                },
                                user_shader_toggle_enabled,
                                &theme,
                            )
                            .when(user_shader_toggle_enabled, |button| {
                                button.on_click(cx.listener(Self::set_user_shader_enabled_click))
                            }),
                        )
                    })
                    .when(user_shader_preview.is_some(), |row| {
                        row.child(
                            control(
                                self,
                                "user-shader-copy",
                                "Copy Exact Text",
                                true,
                                &theme,
                            )
                            .on_click(cx.listener(Self::copy_user_shader_source_text)),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .text_color(color(theme.colors.text_muted))
                    .child("Review the exact source and approve it before enabling. Custom shaders run on macOS; unsupported renderers keep the normal UI."),
            )
            .child(
                div()
                    .relative()
                    .when_some(
                        self.ax_marker("settings-user-shader-status"),
                        |element, marker| element.child(marker),
                    )
                    .w_full()
                    .overflow_hidden()
                    .child(user_shader_status),
            )
            .when_some(user_shader_preview, |panel, shader| {
                let fingerprint = user_shader_fingerprint
                    .clone()
                    .unwrap_or_else(|| "Unavailable: source validation failed".into());
                panel
                    .child(format!("Exact source fingerprint: {fingerprint}"))
                    .child(
                        div()
                            .id("user-shader-source-preview")
                            .w_full()
                            .min_w_0()
                            .max_h(px(280.0))
                            .overflow_scroll()
                            .p(px(theme.spacing.control_gap))
                            .bg(color(theme.colors.surface_raised))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border))
                            .text_color(color(theme.colors.text_primary))
                            .whitespace_nowrap()
                            .child(shader.source.clone()),
                    )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Global Storage"))
                    .child(div().flex_1().child(self.general_settings.storage_label()))
                    .child(
                        control(
                            self,
                            "refresh-global-storage",
                            "Refresh",
                            !maintenance_busy,
                            &theme,
                        )
                        .when(!maintenance_busy, |button| {
                            button.on_click(
                                cx.listener(|view, _, _, cx| view.refresh_global_storage(cx)),
                            )
                        }),
                    ),
            )
            .child(self.render_update_check(&theme, cx))
            .child(self.render_update_payload(&theme, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Maintenance"))
                    .child(
                        control(
                            self,
                            "clear-rebuildable-cache",
                            "Clear Rebuildable Caches",
                            clear_enabled,
                            &theme,
                        )
                        .when(clear_enabled, |button| {
                            button.on_click(
                                cx.listener(|view, _, _, cx| view.clear_rebuildable_cache(cx)),
                            )
                        }),
                    ),
            )
            .when(
                !self.general_settings.maintenance_status.is_empty(),
                |panel| {
                    panel.child(
                        div()
                            .relative()
                            .when_some(
                                self.ax_marker("settings-maintenance-status"),
                                |element, marker| element.child(marker),
                            )
                            .w_full()
                            .overflow_hidden()
                            .child(self.general_settings.maintenance_status.clone()),
                    )
                },
            )
            .when(
                !self.general_settings.status.is_empty()
                    || !self.general_settings.visual_effect_status.is_empty()
                    || !self
                        .general_settings
                        .visual_effect_persistence_status
                        .is_empty(),
                |panel| {
                    panel.child(
                        div()
                            .relative()
                            .when_some(
                                self.ax_marker("settings-general-status"),
                                |element, marker| element.child(marker),
                            )
                            .w_full()
                            .overflow_hidden()
                            .child(
                                [
                                    self.general_settings.status.as_str(),
                                    self.general_settings.visual_effect_status.as_str(),
                                    self.general_settings
                                        .visual_effect_persistence_status
                                        .as_str(),
                                ]
                                .into_iter()
                                .filter(|status| !status.is_empty())
                                .collect::<Vec<_>>()
                                .join(" · "),
                            ),
                    )
                },
            )
    }
}

pub(super) fn control(
    browser: &Browser,
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<Div> {
    let ax_id = match id {
        "general-settings-close" => "settings-close",
        "rating-decay-decrease" => "settings-rating-decay-decrease",
        "rating-decay-increase" => "settings-rating-decay-increase",
        "anti-click-toggle" => "settings-anti-click-toggle",
        "anti-click-fade-decrease" => "settings-anti-click-fade-decrease",
        "anti-click-fade-increase" => "settings-anti-click-fade-increase",
        "choose-trash-folder" => "settings-trash-choose",
        "clear-trash-folder" => "settings-trash-clear",
        "refresh-global-storage" => "settings-storage-refresh",
        "clear-rebuildable-cache" => "settings-cache-clear",
        "visual-effect-preset" => "settings-visual-effect-preset",
        "visual-effect-toggle" => "settings-visual-effect-toggle",
        "visual-effect-strength-decrease" => "settings-visual-effect-strength-decrease",
        "visual-effect-strength-increase" => "settings-visual-effect-strength-increase",
        "user-shader-import" => "settings-user-shader-import",
        "user-shader-cancel" => "settings-user-shader-cancel",
        "user-shader-approve" => "settings-user-shader-approve",
        "user-shader-revoke" => "settings-user-shader-revoke",
        "user-shader-clear" => "settings-user-shader-clear",
        "user-shader-copy" => "settings-user-shader-copy",
        "user-shader-toggle" => "settings-user-shader-toggle",
        _ => id,
    };
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .relative()
        .when_some(browser.ax_focus(ax_id), |element, focus| {
            element.track_focus(&focus)
        })
        .when_some(browser.ax_marker(ax_id), |element, marker| {
            element.child(marker)
        })
        .flex_none()
        .flex()
        .items_center()
        .h(px(theme.spacing.control_height))
        .px(px(theme.spacing.control_gap))
        .border(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border_emphasis))
        .focus({
            let focus_color = color(theme.interaction.focus_outline);
            let focus_width = theme.interaction.focus_outline_width;
            move |style| style.border(px(focus_width)).border_color(focus_color)
        })
        .text_color(color(if enabled {
            theme.colors.text_primary
        } else {
            theme.colors.text_disabled
        }))
        .when(enabled, |button| {
            let hover = color(theme.colors.selection_hover_fill);
            let pressed = color(theme.interaction.pressed_fill);
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
        })
        .child(label)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn read_user_shader_file(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("could not open selected file: {error}"))?;
    read_bounded_user_shader(file)
}

fn read_bounded_user_shader(reader: impl Read) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(MAX_USER_SHADER_SOURCE_BYTES + 1);
    reader
        .take((MAX_USER_SHADER_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read selected file: {error}"))?;
    if bytes.len() > MAX_USER_SHADER_SOURCE_BYTES {
        return Err(format!(
            "source exceeds the {MAX_USER_SHADER_SOURCE_BYTES}-byte limit"
        ));
    }
    let source =
        String::from_utf8(bytes).map_err(|_| "source is not valid UTF-8 text".to_string())?;
    user_shader_content_fingerprint(&source).map_err(|error| error.to_string())?;
    Ok(source)
}

fn confirmed_shader_is_reviewed(confirmed: &UiPreferences, expected_fingerprint: &str) -> bool {
    confirmed
        .user_shader
        .as_ref()
        .and_then(|shader| user_shader_content_fingerprint(&shader.source).ok())
        .is_some_and(|fingerprint| fingerprint == expected_fingerprint)
}

pub(super) fn user_shader_review_status(
    confirmed: Option<&UiPreferences>,
    preview: Option<&UserShaderPreferences>,
    pending_shader_command: bool,
    import_busy: bool,
    import_status: &str,
    settings_status: &str,
) -> String {
    let Some(confirmed) = confirmed else {
        let mut status = if pending_shader_command {
            "Saving source or approval… Approval is not confirmed yet.".into()
        } else {
            format!(
                "Settings are still settling ({settings_status}); approval is not confirmed yet."
            )
        };
        if import_busy {
            status.push_str(" A WGSL file selection or read is also in progress.");
        }
        if !import_status.is_empty() {
            status.push(' ');
            status.push_str(import_status);
        }
        return status;
    };
    let mut local_status = String::new();
    if import_busy {
        local_status.push_str("WGSL file selection or read is in progress.");
    }
    if !import_status.is_empty() {
        if !local_status.is_empty() {
            local_status.push(' ');
        }
        local_status.push_str(import_status);
    }
    let saved_shader = confirmed.user_shader.as_ref();
    let mut status = if let Some(shader) = saved_shader {
        let approved = user_shader_content_fingerprint(&shader.source)
            .ok()
            .is_some_and(|fingerprint| {
                shader.approved_fingerprint.as_deref() == Some(fingerprint.as_str())
            });
        let mut status = if approved {
            "Approval saved for this source. Enable only if you trust this code; approval is not a sandbox.".to_owned()
        } else {
            "Source is saved but not approved. Review the full text before approving.".to_owned()
        };
        if settings_status != "Settings saved" {
            status.push_str(" Last settings event: ");
            status.push_str(settings_status);
        }
        status
    } else if preview.is_some() {
        "New source is only a preview; it is not saved or approved yet.".into()
    } else {
        "No custom WGSL source is saved.".into()
    };
    if !local_status.is_empty() {
        status.push(' ');
        status.push_str(&local_status);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const POST_VERTEX: &str = r#"
struct PostVertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn post_vertex(@builtin(vertex_index) index: u32) -> PostVertex {
    var out: PostVertex;
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[index];
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = vec2<f32>((position.x + 1.0) * 0.5, (1.0 - position.y) * 0.5);
    return out;
}
"#;

    fn validate_generated_shader(effect: ShippedVisualEffectPreferences) {
        let sources = shipped_visual_effect_shaders(effect);
        assert!(!sources.is_empty());
        assert!(sources.len() <= gpui::MAX_POST_SHADER_PASSES);
        for source in sources {
            let module = naga::front::wgsl::parse_str(&format!("{POST_VERTEX}\n{source}"))
                .expect("generated built-in WGSL parses");
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::empty(),
            )
            .validate(&module)
            .expect("generated built-in WGSL validates");
        }
    }

    #[test]
    fn built_in_shaders_are_static_alpha_safe_and_disabled_without_a_pass() {
        let mut effect = ShippedVisualEffectPreferences::default();
        assert!(shipped_visual_effect_shaders(effect).is_empty());

        effect.enabled = true;
        effect.strength_percent = 50;
        let grayscale = shipped_visual_effect_shaders(effect).remove(0);
        assert!(grayscale.contains("vec3<f32>(luma)"));
        assert!(grayscale.contains("mix(color.rgb, vec3<f32>(luma), 0.50), color.a"));

        effect.preset = ShippedVisualEffectPreset::Invert;
        let invert = shipped_visual_effect_shaders(effect).remove(0);
        assert!(invert.contains("vec3<f32>(color.a) - color.rgb"));
        assert!(invert.contains("color.a)"));
        assert!(!invert.contains("arbitrary"));
    }

    #[test]
    fn generated_builtin_shaders_validate_at_both_strength_bounds() {
        for preset in [
            ShippedVisualEffectPreset::Grayscale,
            ShippedVisualEffectPreset::Invert,
            ShippedVisualEffectPreset::GrayscaleThenInvert,
        ] {
            for strength_percent in [0, MAX_VISUAL_EFFECT_STRENGTH_PERCENT] {
                validate_generated_shader(ShippedVisualEffectPreferences {
                    enabled: true,
                    preset,
                    strength_percent,
                });
            }
        }
    }

    #[test]
    fn shipped_two_pass_shader_preserves_order_strength_and_disable_contract() {
        let mut effect = ShippedVisualEffectPreferences {
            enabled: true,
            preset: ShippedVisualEffectPreset::GrayscaleThenInvert,
            strength_percent: 0,
        };
        let sources = shipped_visual_effect_shaders(effect);
        assert_eq!(sources.len(), 2);
        assert!(sources[0].contains("mix(color.rgb, vec3<f32>(luma), 0.00), color.a"));
        assert!(
            sources[1].contains("mix(color.rgb, vec3<f32>(color.a) - color.rgb, 0.00), color.a")
        );
        assert_eq!(visual_effect_strength_label(effect), "0% per pass");
        effect.enabled = false;
        assert!(shipped_visual_effect_shaders(effect).is_empty());
        effect.enabled = true;
        effect.strength_percent = 100;
        assert_eq!(visual_effect_strength_label(effect), "100% per pass");
        let sources = shipped_visual_effect_shaders(effect);
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.contains("1.00), color.a"))
        );
    }

    #[test]
    fn failed_shader_key_is_not_retried_until_configuration_or_qa_revision_changes() {
        let mut settings = GeneralSettings::default();
        let effect = ShippedVisualEffectPreferences {
            enabled: true,
            preset: ShippedVisualEffectPreset::Grayscale,
            strength_percent: 100,
        };
        assert!(settings.should_attempt_visual_effect(effect));
        // Recording the attempt before compilation failure prevents per-frame retries.
        assert!(!settings.should_attempt_visual_effect(effect));

        settings.observe_visual_effect_debug_override(true, 1);
        assert!(settings.visual_effect_last_attempt.is_none());
        settings.observe_visual_effect_debug_override(false, 2);
        // QA turned on and back off between browser renders. The final boolean is
        // unchanged, but the generation proves the renderer was changed out of band.
        assert!(settings.should_attempt_visual_effect(effect));
        assert!(!settings.should_attempt_visual_effect(effect));
    }

    #[test]
    fn local_disable_survives_stale_enabled_snapshot_and_keeps_recovery_status() {
        let mut settings = GeneralSettings::default();
        settings.local_visual_effect_disable_survives_owner_projection();
        settings.mark_local_visual_effect_disable_unpersisted("profile writer unavailable");
        assert!(!settings.effective_visual_effect().enabled);
        assert!(
            settings
                .visual_effect_persistence_status
                .contains("session only")
        );
        assert!(
            settings
                .visual_effect_persistence_status
                .contains("profile writer unavailable")
        );

        // The user explicitly re-enables before the older disable failure arrives.
        settings.visual_effect_local_enabled = None;
        settings.visual_effect_persistence_status.clear();
        settings.mark_local_visual_effect_disable_unpersisted("older disable failed");
        assert!(settings.effective_visual_effect().enabled);
        assert!(settings.visual_effect_persistence_status.is_empty());
    }

    #[test]
    fn user_shader_reader_enforces_size_utf8_and_nonblank_boundaries() {
        let long_line = format!("line\n{}\n", "x".repeat(4096));
        assert_eq!(
            read_bounded_user_shader(Cursor::new(long_line.as_bytes()))
                .expect("long source lines remain intact for review"),
            long_line
        );
        let source = "x".repeat(MAX_USER_SHADER_SOURCE_BYTES);
        assert_eq!(
            read_bounded_user_shader(Cursor::new(source.as_bytes()))
                .expect("exact limit remains readable"),
            source
        );
        let too_large = "x".repeat(MAX_USER_SHADER_SOURCE_BYTES + 1);
        assert!(
            read_bounded_user_shader(Cursor::new(too_large.as_bytes()))
                .expect_err("one byte over the limit is rejected")
                .contains("byte limit")
        );
        assert!(
            read_bounded_user_shader(Cursor::new([0xff]))
                .expect_err("binary data is not UTF-8")
                .contains("UTF-8")
        );
        assert!(
            read_bounded_user_shader(Cursor::new(b" \n\t".as_slice()))
                .expect_err("whitespace-only source is rejected")
                .contains("non-whitespace")
        );
    }

    #[test]
    fn newer_picker_cancellation_and_retirement_fence_old_reads() {
        let mut state = UserShaderImport::default();
        let first = state.begin();
        assert!(state.start_read(first));
        let second = state.begin();
        assert!(!state.is_current(first));
        assert!(!state.finish(first));
        assert!(state.is_current(second));
        assert!(state.start_read(second));
        state.invalidate();
        assert!(!state.finish(second));
        assert!(!state.busy());
    }

    #[test]
    fn approval_token_cannot_approve_a_newer_unreviewed_source() {
        let reviewed_source = "@fragment fn reviewed() {}";
        let reviewed_fingerprint = user_shader_content_fingerprint(reviewed_source).unwrap();
        let mut confirmed = UiPreferences::default();
        confirmed.user_shader = Some(UserShaderPreferences {
            source: "@fragment fn replacement() {}".into(),
            approved_fingerprint: None,
            enabled: false,
        });
        assert!(!confirmed_shader_is_reviewed(
            &confirmed,
            &reviewed_fingerprint
        ));
        let current_fingerprint =
            user_shader_content_fingerprint(&confirmed.user_shader.as_ref().unwrap().source)
                .unwrap();
        assert!(confirmed_shader_is_reviewed(
            &confirmed,
            &current_fingerprint
        ));
    }

    #[test]
    fn optimistic_approval_and_local_messages_never_mask_unknown_owner_state() {
        let source = "@fragment fn post_fragment() {}";
        let optimistic = UserShaderPreferences {
            source: source.into(),
            approved_fingerprint: Some(user_shader_content_fingerprint(source).unwrap()),
            enabled: false,
        };
        let pending = user_shader_review_status(
            None,
            Some(&optimistic),
            true,
            false,
            "Copied the exact source text to the clipboard.",
            "Saving settings…",
        );
        assert!(pending.contains("Approval is not confirmed yet"));
        assert!(pending.contains("Copied the exact source"));
        assert!(!pending.contains("Approval is saved"));

        let failed = user_shader_review_status(
            None,
            Some(&optimistic),
            false,
            false,
            "Import failed; the saved source is unchanged.",
            "Settings persistence closed",
        );
        assert!(failed.contains("approval is not confirmed yet"));
        assert!(failed.contains("Settings persistence closed"));
        assert!(failed.contains("Import failed"));
        assert!(!failed.contains("Approval is saved"));
    }

    #[test]
    fn custom_shader_authority_pause_is_idempotent_and_explicit_disable_stays_latched() {
        let mut runtime = UserShaderRuntime::default();
        runtime.attempted = Some(("sha256:v1:old".into(), 0));
        let first_generation = runtime.generation;
        assert!(runtime.pause_for_unconfirmed_preferences());
        assert!(!runtime.locally_disabled);
        assert!(!runtime.completion_is_current(first_generation, 0));
        let paused_generation = runtime.generation;
        assert!(!runtime.pause_for_unconfirmed_preferences());
        assert_eq!(runtime.generation, paused_generation);
        runtime.suppress_shipped = true;
        assert!(
            runtime.selected(false),
            "pending authority retains the Disable control"
        );
        assert!(runtime.reconcile_unselected_source(true));
        let draining_generation = runtime.generation;
        assert!(!runtime.reconcile_unselected_source(true));
        assert_eq!(runtime.generation, draining_generation);
        assert!(runtime.should_start("sha256:v1:new"));

        runtime.debug_revision = 2;
        runtime.explicitly_disable();
        let disabled_generation = runtime.generation;
        assert!(runtime.locally_disabled);
        assert!(!runtime.selected(true));
        assert!(!runtime.completion_is_current(disabled_generation, 2));
        runtime.explicitly_enable();
        assert!(runtime.completion_is_current(runtime.generation, 2));
        assert!(runtime.selected(true));
        assert!(runtime.should_start("sha256:v1:current"));
        assert!(!runtime.locally_disabled);
        runtime.mark_failed("compile failed".into());
        assert!(!runtime.selected(true));
        assert!(runtime.suppress_shipped);
        assert!(!runtime.should_start("sha256:v1:current"));
        runtime.explicitly_enable();
        assert!(runtime.should_start("sha256:v1:current"));
    }

    #[test]
    fn custom_shader_draining_task_fences_invalidated_generations_and_debug_revisions() {
        let mut runtime = UserShaderRuntime::default();
        runtime.suppress_shipped = true;
        runtime.task = Some(gpui::Task::ready(()));

        let old_generation = runtime.generation;
        runtime.explicitly_disable();
        // Reset can fail to persist after this local barrier is installed.
        // It remains disabled and accurately exposes the enable action.
        assert!(!runtime.selected(true));
        assert!(!runtime.should_start("sha256:v1:new"));
        assert!(!runtime.completion_is_current(old_generation, runtime.debug_revision));

        runtime.explicitly_enable();
        assert!(!runtime.should_start("sha256:v1:new"));
        assert!(runtime.completion_is_current(runtime.generation, runtime.debug_revision));

        let old_debug_revision = runtime.debug_revision;
        runtime.debug_revision = runtime.debug_revision.wrapping_add(1);
        assert!(!runtime.completion_is_current(runtime.generation, old_debug_revision));
        assert!(!runtime.should_start("sha256:v1:new"));

        runtime.task.take();
        assert!(runtime.should_start("sha256:v1:new"));
        assert!(runtime.completion_is_current(runtime.generation, runtime.debug_revision));

        runtime.mark_failed("prepare failed".into());
        assert!(!runtime.should_start("sha256:v1:new"));
        runtime.explicitly_enable();
        assert!(runtime.should_start("sha256:v1:new"));
    }
}
