//! GPUI-free persisted preferences and their ordered session patches.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use yield_core::COLLECTION_COUNT;
use yield_design::{Theme, ThemeError, validate_theme};
use yield_library::sample_sources::UI_PREFERENCES_MAX_BYTES;

use crate::SessionError;

/// The only persisted GPUI-preferences document version understood by this session owner.
pub const UI_PREFERENCES_SCHEMA_VERSION: u32 = 1;
const MAX_THEME_SOURCE_PATH_BYTES: usize = 16 * 1024;
const MAX_AUDIO_OUTPUT_NAME_BYTES: usize = 1024;
/// Maximum UTF-8 source size for one imported user WGSL shader.
pub const MAX_USER_SHADER_SOURCE_BYTES: usize = 64 * 1024;
/// Shader ABI generation included in user-approval fingerprints.
pub const USER_SHADER_CONTRACT_VERSION: u32 = 1;
const USER_SHADER_APPROVAL_DOMAIN_PREFIX: &[u8] = b"PORTALSURFER/Yield/WGSL-approval\0contract-v";
/// Longest persisted custom collection label, measured in UTF-8 bytes.
pub const MAX_COLLECTION_NAME_BYTES: usize = 128;
const OUTPUT_VOLUME_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Default interval between positive-rating decay steps, in weeks.
pub const DEFAULT_RATING_DECAY_WEEKS: u16 = 4;
/// Default in-app text size as a percentage of the authored theme.
pub const DEFAULT_TEXT_SCALE_PERCENT: u16 = 100;
/// Enlarged in-app text size for accessibility verification.
pub const ENLARGED_TEXT_SCALE_PERCENT: u16 = 200;
/// Shortest supported rating-decay interval.
pub const MIN_RATING_DECAY_WEEKS: u16 = 1;
/// Longest supported rating-decay interval.
pub const MAX_RATING_DECAY_WEEKS: u16 = 52;
/// Largest strength value accepted for a shipped visual effect.
pub const MAX_VISUAL_EFFECT_STRENGTH_PERCENT: u16 = 100;

fn default_rating_decay_weeks() -> u16 {
    DEFAULT_RATING_DECAY_WEEKS
}

fn default_help_tooltips_enabled() -> bool {
    true
}

fn default_autoplay_selection() -> bool {
    true
}

fn default_output_volume() -> f32 {
    1.0
}

fn default_text_scale_percent() -> u16 {
    DEFAULT_TEXT_SCALE_PERCENT
}

fn default_collection_names() -> [Option<String>; COLLECTION_COUNT] {
    std::array::from_fn(|_| None)
}

/// One safe visual effect authored and compiled into Yield.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShippedVisualEffectPreset {
    /// Convert the composed window to luma while preserving alpha.
    #[default]
    Grayscale,
    /// Invert premultiplied RGB while preserving alpha.
    Invert,
    /// Two ordered passes: grayscale, then invert; strength applies to each pass.
    GrayscaleThenInvert,
}

/// Persistent selection and per-pass strength for the built-in visual effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShippedVisualEffectPreferences {
    /// Whether the selected effect is active.
    #[serde(default)]
    pub enabled: bool,
    /// Built-in effect selected even while disabled.
    #[serde(default)]
    pub preset: ShippedVisualEffectPreset,
    /// Blend strength, from 0 through 100 percent.
    #[serde(default = "default_visual_effect_strength_percent")]
    pub strength_percent: u16,
}

fn default_visual_effect_strength_percent() -> u16 {
    MAX_VISUAL_EFFECT_STRENGTH_PERCENT
}

impl Default for ShippedVisualEffectPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            preset: ShippedVisualEffectPreset::Grayscale,
            strength_percent: default_visual_effect_strength_percent(),
        }
    }
}

impl ShippedVisualEffectPreferences {
    fn validate(self) -> Result<Self, PreferenceError> {
        if self.strength_percent > MAX_VISUAL_EFFECT_STRENGTH_PERCENT {
            return Err(PreferenceError::InvalidVisualEffectStrength);
        }
        Ok(self)
    }
}

/// Bounded local WGSL content with an explicit profile-owned approval receipt.
///
/// The profile file is trusted local state, not a user-signature service. A matching
/// fingerprint records explicit approval for exact source bytes under one shader ABI;
/// it does not sandbox WGSL or provide GPU execution time limits. Older binaries reject
/// profiles containing this configured field under the existing strict unknown-field policy.
///
/// Contract v1 is the current single-pass window ABI: a user provides only the fragment
/// `post_fragment`; Yield injects its fullscreen `post_vertex`. Group 0 binding 0 is the
/// composed window texture and binding 1 its sampler. There are no uniforms, time/music
/// values, asset bindings, or multipass resources. UV is normalized; source pixels are
/// premultiplied-alpha BGRA8. This gate does not parse, compile, or promise that source is
/// valid or performant; the current consumer compiles for Metal 2.3 with empty Naga
/// capabilities. Explicit approval must precede future execution from a confirmed
/// owner snapshot; source paths, names, and successful parsing cannot approve code.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShaderPreferences {
    /// Exact UTF-8 source bytes last imported by the user.
    pub source: String,
    /// Fingerprint approved for these exact bytes and the current shader contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_fingerprint: Option<String>,
    /// Whether the approved source is selected for the single-pass window effect.
    #[serde(default)]
    pub enabled: bool,
}

impl UserShaderPreferences {
    fn validate(&self) -> Result<(), PreferenceError> {
        let fingerprint = user_shader_content_fingerprint(&self.source)?;
        if self
            .approved_fingerprint
            .as_deref()
            .is_some_and(|approval| approval != fingerprint)
        {
            return Err(PreferenceError::InvalidStoredUserShaderApproval);
        }
        if self.enabled && self.approved_fingerprint.as_deref() != Some(&fingerprint) {
            return Err(PreferenceError::UserShaderNotApproved);
        }
        Ok(())
    }
}

/// Fingerprint exact source UTF-8 bytes for the current documented shader contract.
///
/// The receipt is `sha256:v1:<lowercase-hex>`, hashing the fixed domain prefix
/// `PORTALSURFER/Yield/WGSL-approval\0contract-v`, the decimal contract version, a NUL,
/// and the exact source bytes.
/// Any shader ABI change must increment the contract version and requires reapproval.
pub fn user_shader_content_fingerprint(source: &str) -> Result<String, PreferenceError> {
    validate_user_shader_source(source)?;
    let mut hasher = Sha256::new();
    hasher.update(USER_SHADER_APPROVAL_DOMAIN_PREFIX);
    hasher.update(USER_SHADER_CONTRACT_VERSION.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(source.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(format!("sha256:v{USER_SHADER_CONTRACT_VERSION}:{hex}"))
}

fn validate_user_shader_source(source: &str) -> Result<(), PreferenceError> {
    if source.len() > MAX_USER_SHADER_SOURCE_BYTES {
        return Err(PreferenceError::UserShaderSourceTooLarge);
    }
    if source.trim().is_empty() {
        return Err(PreferenceError::UserShaderSourceEmpty);
    }
    Ok(())
}

/// The optional, renderer-neutral audio output routing persisted with the UI preferences.
///
/// The session owner validates the shape of these values but does not probe or enumerate
/// platform audio devices. An absent field lets the playback backend choose its default.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioOutputPreferences {
    /// Optional host or driver name selected by the user.
    #[serde(default)]
    pub host: Option<String>,
    /// Optional device name selected by the user.
    #[serde(default)]
    pub device: Option<String>,
    /// Optional requested sample rate in hertz.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Optional requested output buffer size in frames.
    #[serde(default)]
    pub buffer_size: Option<u32>,
}

impl AudioOutputPreferences {
    fn validate(&self) -> Result<(), PreferenceError> {
        validate_audio_output_name(self.host.as_deref(), "host")?;
        validate_audio_output_name(self.device.as_deref(), "device")?;
        if self.sample_rate == Some(0) {
            return Err(PreferenceError::InvalidAudioOutputValue {
                field: "sample_rate",
            });
        }
        if self.buffer_size == Some(0) {
            return Err(PreferenceError::InvalidAudioOutputValue {
                field: "buffer_size",
            });
        }
        Ok(())
    }
}

/// Deadline gate used by the session worker to coalesce rapid output-volume edits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PreferenceSaveDeadline(Option<Instant>);

impl PreferenceSaveDeadline {
    pub(crate) fn defer_output_volume(&mut self, now: Instant) {
        self.0 = Some(now + OUTPUT_VOLUME_SAVE_DEBOUNCE);
    }

    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    pub(crate) fn permits_save(self, now: Instant, bypass: bool) -> bool {
        bypass || self.0.is_none_or(|deadline| now >= deadline)
    }
}

/// The complete GPUI preference document stored in the profile-owned library metadata table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreferences {
    /// Version of this preference document.
    pub schema_version: u32,
    /// Complete validated renderer-neutral theme document.
    pub theme: Theme,
    /// Optional absolute path used by an explicit theme reload action.
    #[serde(default)]
    pub theme_source_path: Option<PathBuf>,
    /// In-app text scale; the authored theme remains unchanged.
    #[serde(default = "default_text_scale_percent")]
    pub text_scale_percent: u16,
    /// Whether a newly focused sample may start playback.
    #[serde(default = "default_autoplay_selection")]
    pub autoplay_selection: bool,
    /// Interval between positive-rating decay steps, in weeks.
    #[serde(default = "default_rating_decay_weeks")]
    pub rating_decay_weeks: u16,
    /// Optional destination for physical trash operations; persistence grants no file authority.
    #[serde(default)]
    pub trash_folder: Option<PathBuf>,
    /// Whether hover help is shown throughout the browser.
    #[serde(default = "default_help_tooltips_enabled")]
    pub help_tooltips_enabled: bool,
    /// Whether audition playback loops its active region.
    #[serde(default)]
    pub loop_enabled: bool,
    /// Independent locked loop choice: absent means ordinary per-file loop policy applies.
    #[serde(default)]
    pub loop_lock_override: Option<bool>,
    /// Whether audition playback uses the raw peak of its active span to normalize gain.
    #[serde(default)]
    pub normalized_audition_enabled: bool,
    /// Linear output volume applied to audition playback.
    #[serde(default = "default_output_volume")]
    pub output_volume: f32,
    /// Optional output routing requested by the user.
    #[serde(default)]
    pub audio_output: AudioOutputPreferences,
    /// Profile-owned custom labels for the six stable collection slots.
    #[serde(default = "default_collection_names")]
    pub collection_names: [Option<String>; COLLECTION_COUNT],
    /// Opt-in built-in window effect; old preference documents default to disabled grayscale.
    #[serde(default)]
    pub shipped_visual_effect: ShippedVisualEffectPreferences,
    /// Optional exact-content user shader state; absent unless explicitly imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_shader: Option<UserShaderPreferences>,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            schema_version: UI_PREFERENCES_SCHEMA_VERSION,
            theme: Theme::default(),
            theme_source_path: None,
            text_scale_percent: default_text_scale_percent(),
            autoplay_selection: true,
            help_tooltips_enabled: true,
            rating_decay_weeks: DEFAULT_RATING_DECAY_WEEKS,
            trash_folder: None,
            loop_enabled: false,
            loop_lock_override: None,
            normalized_audition_enabled: false,
            output_volume: 1.0,
            audio_output: AudioOutputPreferences::default(),
            collection_names: default_collection_names(),
            shipped_visual_effect: ShippedVisualEffectPreferences::default(),
            user_shader: None,
        }
    }
}

impl UiPreferences {
    /// Return source only when enabled and approved for its exact bytes and current contract.
    ///
    /// Future execution code must call this only on a confirmed owner snapshot, not an
    /// optimistic UI projection. Approval is not a WGSL sandbox or GPU time limit.
    pub fn enabled_user_shader_source(&self) -> Option<&str> {
        let user_shader = self.user_shader.as_ref()?;
        if !user_shader.enabled {
            return None;
        }
        let fingerprint = user_shader_content_fingerprint(&user_shader.source).ok()?;
        (user_shader.approved_fingerprint.as_deref() == Some(&fingerprint))
            .then_some(user_shader.source.as_str())
    }

    /// Validate the complete preference document without performing filesystem I/O.
    pub fn validate(self) -> Result<Self, PreferenceError> {
        if self.schema_version != UI_PREFERENCES_SCHEMA_VERSION {
            return Err(PreferenceError::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        let theme = validate_theme(self.theme)
            .map_err(|error| PreferenceError::InvalidTheme(error.to_string()))?;
        validate_text_scale_percent(self.text_scale_percent)?;
        if let Some(path) = self.theme_source_path.as_deref() {
            validate_theme_source_path(path)?;
        }
        validate_rating_decay_weeks(self.rating_decay_weeks)?;
        validate_trash_folder(self.trash_folder.as_deref())?;
        validate_output_volume(self.output_volume)?;
        self.audio_output.validate()?;
        self.shipped_visual_effect.validate()?;
        if let Some(user_shader) = self.user_shader.as_ref() {
            user_shader.validate()?;
        }
        for name in self.collection_names.iter().flatten() {
            validate_collection_name(name)?;
        }
        Ok(Self { theme, ..self })
    }

    /// Decode and validate one bounded JSON payload from the profile owner.
    pub fn from_payload(payload: &str) -> Result<Self, PreferenceError> {
        if payload.len() > UI_PREFERENCES_MAX_BYTES {
            return Err(PreferenceError::PayloadTooLarge {
                actual: payload.len(),
            });
        }
        let preferences: Self = serde_json::from_str(payload)
            .map_err(|error| PreferenceError::Json(error.to_string()))?;
        preferences.validate()
    }

    /// Validate and encode this document for the profile owner.
    pub fn to_payload(&self) -> Result<String, PreferenceError> {
        let preferences = self.clone().validate()?;
        let payload = serde_json::to_string(&preferences)
            .map_err(|error| PreferenceError::Json(error.to_string()))?;
        if payload.len() > UI_PREFERENCES_MAX_BYTES {
            return Err(PreferenceError::PayloadTooLarge {
                actual: payload.len(),
            });
        }
        Ok(payload)
    }
}

/// Validation failures for a preference document or patch.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PreferenceError {
    /// The stored document schema is not supported.
    #[error("unsupported UI preferences schema version {actual}")]
    UnsupportedSchema {
        /// Stored schema version.
        actual: u32,
    },
    /// The embedded theme failed its renderer-neutral validation rules.
    #[error("invalid UI preferences theme: {0}")]
    InvalidTheme(String),
    /// The optional theme reload path was not an absolute path.
    #[error("theme source path must be absolute: {0}")]
    ThemeSourcePathNotAbsolute(PathBuf),
    /// The optional theme reload path could not be represented in the UTF-8 JSON payload.
    #[error("theme source path must be valid UTF-8")]
    ThemeSourcePathNotUtf8,
    /// The optional theme reload path exceeded the bounded payload policy.
    #[error("theme source path exceeds the {MAX_THEME_SOURCE_PATH_BYTES}-byte limit")]
    ThemeSourcePathTooLarge,
    /// The decay interval was outside the reference application's supported range.
    #[error("rating decay must be between 1 and 52 weeks")]
    InvalidRatingDecay,
    /// The configured trash destination was not a bounded absolute normal path.
    #[error(
        "trash folder must be an absolute UTF-8 path without parent traversal or NUL, at most 16384 bytes"
    )]
    InvalidTrashFolder,
    /// The output volume was not a finite value in the inclusive 0..=1 range.
    #[error("output volume must be finite and between 0 and 1")]
    InvalidOutputVolume,
    /// The in-app text scale is not one of the supported sizes.
    #[error("text scale must be 100% or 200%")]
    InvalidTextScale,
    /// A shipped visual effect strength was outside the inclusive 0..=100 range.
    #[error("visual effect strength must be between 0 and 100 percent")]
    InvalidVisualEffectStrength,
    /// Imported source contained no non-whitespace text.
    #[error("user shader source must contain non-whitespace UTF-8 text")]
    UserShaderSourceEmpty,
    /// Imported source exceeded the raw UTF-8 source limit.
    #[error("user shader source exceeds the {MAX_USER_SHADER_SOURCE_BYTES}-byte UTF-8 limit")]
    UserShaderSourceTooLarge,
    /// Approval was requested before importing source.
    #[error("import a user shader source before approving it")]
    UserShaderNotImported,
    /// Caller-confirmed fingerprint does not match the currently imported exact bytes.
    #[error("approval fingerprint does not match the current user shader source")]
    UserShaderApprovalMismatch,
    /// A persisted receipt is malformed, stale, or belongs to an older shader contract.
    #[error("stored user shader approval is invalid; re-import and explicitly approve the source")]
    InvalidStoredUserShaderApproval,
    /// Enable requires a matching explicit approval for the exact current source.
    #[error("approve the current user shader source before enabling it")]
    UserShaderNotApproved,
    /// An audio output host or device name was empty, contained a NUL, or exceeded its bound.
    #[error(
        "audio output {field} must be non-empty, contain no NUL bytes, and be at most {MAX_AUDIO_OUTPUT_NAME_BYTES} bytes"
    )]
    InvalidAudioOutputName {
        /// The invalid audio output field name.
        field: &'static str,
    },
    /// An audio output numeric setting was zero even though it is optional and positive.
    #[error("audio output {field} must be greater than zero when provided")]
    InvalidAudioOutputValue {
        /// The invalid audio output field name.
        field: &'static str,
    },
    /// A collection label did not meet the bounded single-line name contract.
    #[error(
        "collection name must be trimmed, non-empty, single-line, NUL-free, and at most {MAX_COLLECTION_NAME_BYTES} UTF-8 bytes"
    )]
    InvalidCollectionName,
    /// The requested stable collection slot does not exist.
    #[error("collection slot must be less than {COLLECTION_COUNT}")]
    InvalidCollectionSlot,
    /// The profile payload exceeded its bounded UTF-8 byte limit.
    #[error(
        "UI preferences payload exceeds the {UI_PREFERENCES_MAX_BYTES}-byte limit ({actual} bytes)"
    )]
    PayloadTooLarge {
        /// Payload size in bytes.
        actual: usize,
    },
    /// The profile payload was not valid JSON.
    #[error("invalid UI preferences JSON: {0}")]
    Json(String),
    /// The persisted payload is invalid and must be reset before editing.
    #[error("stored UI preferences are invalid; reset preferences before editing")]
    StoredPayloadInvalid,
}

impl From<ThemeError> for PreferenceError {
    fn from(error: ThemeError) -> Self {
        Self::InvalidTheme(error.to_string())
    }
}

fn validate_theme_source_path(path: &Path) -> Result<(), PreferenceError> {
    if !path.is_absolute() {
        return Err(PreferenceError::ThemeSourcePathNotAbsolute(
            path.to_path_buf(),
        ));
    }
    let value = path
        .to_str()
        .ok_or(PreferenceError::ThemeSourcePathNotUtf8)?;
    if value.len() > MAX_THEME_SOURCE_PATH_BYTES {
        return Err(PreferenceError::ThemeSourcePathTooLarge);
    }
    Ok(())
}

fn validate_output_volume(volume: f32) -> Result<(), PreferenceError> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err(PreferenceError::InvalidOutputVolume);
    }
    Ok(())
}

fn validate_text_scale_percent(percent: u16) -> Result<(), PreferenceError> {
    if matches!(
        percent,
        DEFAULT_TEXT_SCALE_PERCENT | ENLARGED_TEXT_SCALE_PERCENT
    ) {
        Ok(())
    } else {
        Err(PreferenceError::InvalidTextScale)
    }
}

fn validate_audio_output_name(
    value: Option<&str>,
    field: &'static str,
) -> Result<(), PreferenceError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty()
        || value.as_bytes().contains(&0)
        || value.len() > MAX_AUDIO_OUTPUT_NAME_BYTES
    {
        return Err(PreferenceError::InvalidAudioOutputName { field });
    }
    Ok(())
}

fn validate_rating_decay_weeks(weeks: u16) -> Result<(), PreferenceError> {
    if !(MIN_RATING_DECAY_WEEKS..=MAX_RATING_DECAY_WEEKS).contains(&weeks) {
        return Err(PreferenceError::InvalidRatingDecay);
    }
    Ok(())
}

fn validate_collection_name(name: &str) -> Result<(), PreferenceError> {
    if name.is_empty()
        || name.trim() != name
        || name.len() > MAX_COLLECTION_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(PreferenceError::InvalidCollectionName);
    }
    Ok(())
}

fn validate_trash_folder(path: Option<&Path>) -> Result<(), PreferenceError> {
    if let Some(path) = path {
        let text = path.to_str().ok_or(PreferenceError::InvalidTrashFolder)?;
        if !path.is_absolute()
            || text.len() > 16384
            || text.contains('\0')
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(PreferenceError::InvalidTrashFolder);
        }
    }
    Ok(())
}

/// One typed user intent applied to the loaded preference document.
#[derive(Clone, Debug)]
pub enum PreferencePatch {
    /// Replace the complete theme and remember its source path for explicit reloads.
    ReplaceTheme {
        /// Complete validated theme document.
        theme: Theme,
        /// Optional absolute source path used for a later reload.
        source_path: Option<PathBuf>,
    },
    /// Change only the motion-enabled flag inside the current theme.
    SetMotionEnabled(bool),
    /// Change the in-app text scale without editing the authored theme.
    SetTextScalePercent(u16),
    /// Change the selection autoplay preference.
    SetAutoplaySelection(bool),
    /// Change the reference rating-decay interval.
    SetRatingDecayWeeks(u16),
    /// Set or clear the configured trash destination; file operations resolve authority separately.
    SetTrashFolder(Option<PathBuf>),
    /// Change global hover-help visibility.
    SetHelpTooltipsEnabled(bool),
    /// Change the audition loop preference.
    SetLoopEnabled(bool),
    /// Enter, cycle, or leave the locked loop override without writing sample metadata.
    SetLoopLockOverride(Option<bool>),
    /// Change the peak-normalized audition preference.
    SetNormalizedAuditionEnabled(bool),
    /// Change the linear audition output volume.
    SetOutputVolume(f32),
    /// Replace all optional audio output routing fields atomically.
    SetAudioOutput(AudioOutputPreferences),
    /// Assign a custom display label to one stable collection slot.
    SetCollectionName {
        /// Zero-based slot identity, independent of active source.
        slot: usize,
        /// Trimmed, bounded user label.
        name: String,
    },
    /// Replace the selected built-in visual effect configuration.
    SetShippedVisualEffect(ShippedVisualEffectPreferences),
    /// Import or replace WGSL text; replacement always clears approval and disables it.
    ImportUserShader {
        /// Exact UTF-8 WGSL text.
        source: String,
    },
    /// Approve the currently imported source after the caller confirms its fingerprint.
    ApproveUserShader {
        /// Expected exact-content fingerprint shown to the caller.
        expected_fingerprint: String,
    },
    /// Enable or disable imported WGSL; enable requires a matching approval.
    SetUserShaderEnabled(bool),
    /// Revoke approval and disable the retained source.
    RevokeUserShaderApproval,
    /// Remove the retained source and all approval state.
    ClearUserShader,
    /// Replace the invalid or current document with built-in defaults.
    Reset,
}

impl PreferencePatch {
    fn apply(self, preferences: &mut UiPreferences) -> Result<bool, PreferenceError> {
        let before = preferences.clone();
        match self {
            Self::ReplaceTheme { theme, source_path } => {
                let theme = validate_theme(theme)?;
                if let Some(path) = source_path.as_deref() {
                    validate_theme_source_path(path)?;
                }
                preferences.theme = theme;
                preferences.theme_source_path = source_path;
            }
            Self::SetMotionEnabled(enabled) => preferences.theme.motion.enabled = enabled,
            Self::SetTextScalePercent(percent) => {
                validate_text_scale_percent(percent)?;
                preferences.text_scale_percent = percent;
            }
            Self::SetAutoplaySelection(enabled) => preferences.autoplay_selection = enabled,
            Self::SetRatingDecayWeeks(weeks) => {
                validate_rating_decay_weeks(weeks)?;
                preferences.rating_decay_weeks = weeks;
            }
            Self::SetTrashFolder(path) => {
                validate_trash_folder(path.as_deref())?;
                preferences.trash_folder = path;
            }
            Self::SetHelpTooltipsEnabled(enabled) => preferences.help_tooltips_enabled = enabled,
            Self::SetLoopEnabled(enabled) => {
                preferences.loop_enabled = enabled;
                preferences.loop_lock_override = None;
            }
            Self::SetLoopLockOverride(locked_override) => {
                preferences.loop_lock_override = locked_override;
            }
            Self::SetNormalizedAuditionEnabled(enabled) => {
                preferences.normalized_audition_enabled = enabled;
            }
            Self::SetOutputVolume(volume) => {
                validate_output_volume(volume)?;
                preferences.output_volume = volume;
            }
            Self::SetAudioOutput(audio_output) => {
                audio_output.validate()?;
                preferences.audio_output = audio_output;
            }
            Self::SetCollectionName { slot, name } => {
                validate_collection_name(&name)?;
                let Some(current) = preferences.collection_names.get_mut(slot) else {
                    return Err(PreferenceError::InvalidCollectionSlot);
                };
                *current = Some(name);
            }
            Self::SetShippedVisualEffect(effect) => {
                preferences.shipped_visual_effect = effect.validate()?;
            }
            Self::ImportUserShader { source } => {
                validate_user_shader_source(&source)?;
                preferences.user_shader = Some(UserShaderPreferences {
                    source,
                    approved_fingerprint: None,
                    enabled: false,
                });
            }
            Self::ApproveUserShader {
                expected_fingerprint,
            } => {
                let Some(user_shader) = preferences.user_shader.as_mut() else {
                    return Err(PreferenceError::UserShaderNotImported);
                };
                let current = user_shader_content_fingerprint(&user_shader.source)?;
                if expected_fingerprint != current {
                    return Err(PreferenceError::UserShaderApprovalMismatch);
                }
                user_shader.approved_fingerprint = Some(current);
            }
            Self::SetUserShaderEnabled(enabled) => {
                if enabled {
                    let Some(user_shader) = preferences.user_shader.as_mut() else {
                        return Err(PreferenceError::UserShaderNotImported);
                    };
                    user_shader.validate()?;
                    let current = user_shader_content_fingerprint(&user_shader.source)?;
                    if user_shader.approved_fingerprint.as_deref() != Some(&current) {
                        return Err(PreferenceError::UserShaderNotApproved);
                    }
                    user_shader.enabled = true;
                } else if let Some(user_shader) = preferences.user_shader.as_mut() {
                    user_shader.enabled = false;
                }
            }
            Self::RevokeUserShaderApproval => {
                if let Some(user_shader) = preferences.user_shader.as_mut() {
                    user_shader.enabled = false;
                    user_shader.approved_fingerprint = None;
                }
            }
            Self::ClearUserShader => preferences.user_shader = None,
            Self::Reset => *preferences = Self::default_preferences(),
        }
        Ok(before != *preferences)
    }

    fn is_reset(&self) -> bool {
        matches!(self, Self::Reset)
    }

    /// Return whether this is one of the explicit user-shader profile commands.
    pub fn is_user_shader_command(&self) -> bool {
        matches!(
            self,
            Self::ImportUserShader { .. }
                | Self::ApproveUserShader { .. }
                | Self::SetUserShaderEnabled(_)
                | Self::RevokeUserShaderApproval
                | Self::ClearUserShader
        )
    }

    /// Apply the command to an immutable input and validate the complete candidate snapshot.
    ///
    /// For shader commands, or snapshots that retain imported shader content, the result is
    /// rejected if its serialized aggregate exceeds the profile payload bound, before a profile
    /// owner can replace its last-good in-memory state. Profiles without shader content retain
    /// their existing size behavior. UI projection can reuse this exact command behavior, but
    /// remains optimistic and grants no execution authority.
    pub fn project(&self, current: &UiPreferences) -> Result<UiPreferences, PreferenceError> {
        let shader_command = self.is_user_shader_command();
        let mut candidate = current.clone();
        self.clone().apply(&mut candidate)?;
        let candidate = candidate.validate()?;
        if shader_command || candidate.user_shader.is_some() {
            let _ = candidate.to_payload()?;
        }
        Ok(candidate)
    }

    fn default_preferences() -> UiPreferences {
        UiPreferences::default()
    }
}

/// A monotonically increasing preference revision.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct PreferenceRevision(u64);

impl PreferenceRevision {
    /// Return the numeric revision for UI fencing and diagnostics.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An id assigned to one submitted preference patch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreferenceRequestId(u64);

impl PreferenceRequestId {
    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric request id for UI correlation.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Events emitted by the session owner for preference state.
#[derive(Debug)]
pub enum PreferenceEvent {
    /// The profile owner is loading the initial preference payload.
    Loading {
        /// Revision associated with the loading state.
        revision: PreferenceRevision,
    },
    /// A complete valid preference snapshot is ready.
    Snapshot {
        /// Revision represented by the snapshot.
        revision: PreferenceRevision,
        /// Immutable preference snapshot.
        preferences: UiPreferences,
    },
    /// A preference revision was durably replaced in the profile metadata row.
    Persisted {
        /// Revision durably written by the library owner.
        revision: PreferenceRevision,
    },
    /// A submitted patch was applied to the in-memory preference projection.
    RequestApplied {
        /// Request that was applied, including an idempotent no-op.
        request: PreferenceRequestId,
        /// Projection revision after applying the request.
        revision: PreferenceRevision,
    },
    /// A submitted patch was rejected without changing the stored payload.
    RequestFailed {
        /// Request that failed.
        request: PreferenceRequestId,
        /// Failure reported by the session owner.
        error: SessionError,
    },
    /// The stored payload was malformed or otherwise failed preference validation.
    LoadFailed {
        /// Validation failure retained while the malformed payload remains untouched.
        error: SessionError,
    },
    /// Preference persistence is unavailable. A shared owner failure is mirrored here while
    /// source commands are also failed closed; a preference-only write failure leaves sources
    /// available.
    PersistenceUnavailable {
        /// Last preference revision known to the session.
        revision: PreferenceRevision,
        /// Preference-lane failure. Shared owner failures are also mirrored to the registry event
        /// stream; preference-only failures leave source persistence available.
        error: SessionError,
    },
    /// The worker has drained and closed.
    Closed,
}

#[derive(Clone)]
struct PendingPatch {
    request: PreferenceRequestId,
    patch: PreferencePatch,
}

/// In-memory preference projection owned by the session worker.
pub(crate) struct PreferenceCoordinator {
    loaded: bool,
    valid: bool,
    baseline: UiPreferences,
    current: UiPreferences,
    edits: Vec<PendingPatch>,
    revision: PreferenceRevision,
    durable_revision: PreferenceRevision,
    force_save: bool,
}

impl PreferenceCoordinator {
    pub(crate) fn new() -> Self {
        let defaults = UiPreferences::default();
        Self {
            loaded: false,
            valid: false,
            baseline: defaults.clone(),
            current: defaults.clone(),
            edits: Vec::new(),
            revision: PreferenceRevision::default(),
            durable_revision: PreferenceRevision::default(),
            force_save: false,
        }
    }

    pub(crate) fn loaded(&self) -> bool {
        self.loaded
    }

    pub(crate) fn valid(&self) -> bool {
        self.valid
    }

    pub(crate) fn revision(&self) -> PreferenceRevision {
        self.revision
    }

    pub(crate) fn snapshot(&self) -> UiPreferences {
        self.current.clone()
    }

    pub(crate) fn queue_patch(&mut self, request: PreferenceRequestId, patch: PreferencePatch) {
        self.edits.push(PendingPatch { request, patch });
    }

    pub(crate) fn clear_pending(&mut self) {
        self.edits.clear();
    }

    pub(crate) fn load_missing(&mut self) -> Vec<PreferenceOutcome> {
        self.install_loaded(UiPreferences::default(), true, false)
    }

    pub(crate) fn load_valid(&mut self, preferences: UiPreferences) -> Vec<PreferenceOutcome> {
        self.install_loaded(preferences, true, false)
    }

    pub(crate) fn load_invalid(&mut self) -> Vec<PreferenceOutcome> {
        self.loaded = true;
        self.valid = false;
        self.baseline = UiPreferences::default();
        self.current = self.baseline.clone();
        self.revision = PreferenceRevision::default();
        self.durable_revision = PreferenceRevision::default();
        self.force_save = false;
        self.apply_queued()
    }

    pub(crate) fn apply_loaded_patch(
        &mut self,
        request: PreferenceRequestId,
        patch: PreferencePatch,
    ) -> Result<bool, PreferenceError> {
        if !self.loaded {
            self.queue_patch(request, patch);
            return Ok(false);
        }
        self.apply_patch(request, patch)
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.force_save || self.revision != self.durable_revision
    }

    pub(crate) fn accept_save_ack(&mut self, revision: PreferenceRevision) -> bool {
        if revision != self.revision {
            return false;
        }
        self.durable_revision = revision;
        self.force_save = false;
        true
    }

    fn install_loaded(
        &mut self,
        preferences: UiPreferences,
        valid: bool,
        force_save: bool,
    ) -> Vec<PreferenceOutcome> {
        self.loaded = true;
        self.valid = valid;
        self.baseline = preferences.clone();
        self.current = preferences.clone();
        self.revision = PreferenceRevision::default();
        self.durable_revision = PreferenceRevision::default();
        self.force_save = force_save;
        self.apply_queued()
    }

    fn apply_queued(&mut self) -> Vec<PreferenceOutcome> {
        let edits = std::mem::take(&mut self.edits);
        edits
            .into_iter()
            .map(|edit| {
                let output_volume_patch =
                    matches!(&edit.patch, PreferencePatch::SetOutputVolume(_));
                let result = self.apply_patch(edit.request, edit.patch);
                let output_volume_changed = output_volume_patch && matches!(&result, Ok(true));
                PreferenceOutcome {
                    request: edit.request,
                    result,
                    output_volume_changed,
                }
            })
            .collect()
    }

    fn apply_patch(
        &mut self,
        request: PreferenceRequestId,
        patch: PreferencePatch,
    ) -> Result<bool, PreferenceError> {
        if !self.valid && !patch.is_reset() {
            return Err(PreferenceError::StoredPayloadInvalid);
        }
        let was_invalid = !self.valid;
        let is_reset = patch.is_reset();
        let candidate = patch.project(&self.current)?;
        let changed = candidate != self.current;
        self.current = candidate;
        let recovered = is_reset && was_invalid;
        if recovered {
            self.valid = true;
            self.baseline = self.current.clone();
            self.force_save = true;
        }
        if changed || recovered {
            self.revision = PreferenceRevision(self.revision.0.saturating_add(1));
        }
        let _ = request;
        Ok(changed || recovered)
    }
}

pub(crate) struct PreferenceOutcome {
    pub(crate) request: PreferenceRequestId,
    pub(crate) result: Result<bool, PreferenceError>,
    pub(crate) output_volume_changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use yield_design::Color;

    #[test]
    fn payload_round_trip_validates_full_theme_and_path() {
        let mut preferences = UiPreferences::default();
        preferences.theme.name = String::from("Saved");
        preferences.theme.colors.accent = Color::rgb(1, 2, 3);
        preferences.theme_source_path = Some(std::env::temp_dir().join("custom theme.json"));
        preferences.autoplay_selection = false;
        preferences.loop_enabled = true;
        preferences.loop_lock_override = Some(false);
        preferences.normalized_audition_enabled = true;
        preferences.output_volume = 0.37;

        let payload = preferences.to_payload().expect("encode preferences");
        assert_eq!(
            UiPreferences::from_payload(&payload).expect("decode"),
            preferences
        );
    }

    #[test]
    fn shipped_two_pass_preset_round_trips_through_profile_owner() {
        assert_eq!(
            serde_json::to_string(&ShippedVisualEffectPreset::Grayscale).unwrap(),
            "\"grayscale\""
        );
        assert_eq!(
            serde_json::to_string(&ShippedVisualEffectPreset::Invert).unwrap(),
            "\"invert\""
        );
        let effect = ShippedVisualEffectPreferences {
            enabled: true,
            preset: ShippedVisualEffectPreset::GrayscaleThenInvert,
            strength_percent: 80,
        };
        let mut owner = PreferenceCoordinator::new();
        owner.load_valid(UiPreferences::default());
        owner
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetShippedVisualEffect(effect),
            )
            .expect("owner accepts compound preset");
        let payload = owner
            .snapshot()
            .to_payload()
            .expect("persistable owner state");
        let loaded = UiPreferences::from_payload(&payload).expect("reload compound preset");
        assert_eq!(loaded.shipped_visual_effect, effect);
        assert!(payload.contains("grayscale_then_invert"));
        assert_eq!(
            UiPreferences::default().shipped_visual_effect,
            ShippedVisualEffectPreferences::default()
        );
    }

    #[test]
    fn shipped_visual_effect_is_legacy_defaulted_validated_and_profile_owned() {
        let mut legacy = serde_json::to_value(UiPreferences::default()).expect("encode");
        legacy
            .as_object_mut()
            .expect("preferences object")
            .remove("shipped_visual_effect");
        let legacy = serde_json::to_string(&legacy).expect("legacy JSON");
        let loaded = UiPreferences::from_payload(&legacy).expect("legacy preferences");
        assert_eq!(
            loaded.shipped_visual_effect,
            ShippedVisualEffectPreferences::default()
        );

        let effect = ShippedVisualEffectPreferences {
            enabled: true,
            preset: ShippedVisualEffectPreset::Invert,
            strength_percent: 70,
        };
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_valid(loaded);
        assert!(
            coordinator
                .apply_loaded_patch(
                    PreferenceRequestId(1),
                    PreferencePatch::SetShippedVisualEffect(effect),
                )
                .expect("owner accepts effect patch")
        );
        assert_eq!(coordinator.snapshot().shipped_visual_effect, effect);
        let durable_payload = coordinator.snapshot().to_payload().expect("owner payload");
        assert_eq!(
            UiPreferences::from_payload(&durable_payload)
                .expect("reloaded profile payload")
                .shipped_visual_effect,
            effect
        );

        let mut invalid_strength = serde_json::to_value(UiPreferences::default()).unwrap();
        invalid_strength["shipped_visual_effect"]["strength_percent"] = 101.into();
        assert_eq!(
            UiPreferences::from_payload(&invalid_strength.to_string()),
            Err(PreferenceError::InvalidVisualEffectStrength)
        );

        let mut unknown_preset = serde_json::to_value(UiPreferences::default()).unwrap();
        unknown_preset["shipped_visual_effect"]["preset"] = "arbitrary_shader".into();
        assert!(matches!(
            UiPreferences::from_payload(&unknown_preset.to_string()),
            Err(PreferenceError::Json(_))
        ));
    }

    #[test]
    fn collection_names_are_bounded_slot_stable_and_round_trip() {
        let mut preferences = UiPreferences::default();
        assert!(
            PreferencePatch::SetCollectionName {
                slot: 2,
                name: "Percussion".to_string(),
            }
            .apply(&mut preferences)
            .expect("rename slot")
        );
        assert_eq!(
            preferences.collection_names[2].as_deref(),
            Some("Percussion")
        );
        assert!(
            preferences
                .collection_names
                .iter()
                .enumerate()
                .all(|(slot, name)| { slot == 2 || name.is_none() })
        );
        assert_eq!(
            UiPreferences::from_payload(&preferences.to_payload().unwrap()).unwrap(),
            preferences
        );
        assert_eq!(
            PreferencePatch::SetCollectionName {
                slot: COLLECTION_COUNT,
                name: "Wrong".to_string(),
            }
            .apply(&mut preferences),
            Err(PreferenceError::InvalidCollectionSlot)
        );
        for invalid in [
            "",
            " Name",
            "Name ",
            "two\nlines",
            "two\twords",
            "bad\0name",
            &"x".repeat(129),
        ] {
            assert_eq!(
                PreferencePatch::SetCollectionName {
                    slot: 2,
                    name: invalid.to_string(),
                }
                .apply(&mut preferences),
                Err(PreferenceError::InvalidCollectionName)
            );
        }
        assert_eq!(
            preferences.collection_names[2].as_deref(),
            Some("Percussion")
        );
        PreferencePatch::Reset.apply(&mut preferences).unwrap();
        assert!(preferences.collection_names.iter().all(Option::is_none));
    }

    #[test]
    fn normalized_audition_defaults_off_for_legacy_preferences_and_resets() {
        let mut payload = serde_json::to_value(UiPreferences::default()).expect("encode");
        payload
            .as_object_mut()
            .expect("object")
            .remove("normalized_audition_enabled");
        let payload = serde_json::to_string(&payload).expect("legacy payload");
        let mut preferences = UiPreferences::from_payload(&payload).expect("legacy decode");
        assert!(!preferences.normalized_audition_enabled);
        assert!(
            PreferencePatch::SetNormalizedAuditionEnabled(true)
                .apply(&mut preferences)
                .unwrap()
        );
        assert!(preferences.normalized_audition_enabled);
        assert!(
            !PreferencePatch::SetNormalizedAuditionEnabled(true)
                .apply(&mut preferences)
                .unwrap()
        );
        PreferencePatch::Reset.apply(&mut preferences).unwrap();
        assert!(!preferences.normalized_audition_enabled);
    }

    #[test]
    fn locked_loop_choice_is_separate_and_normal_toggle_exits_it_atomically() {
        let mut preferences = UiPreferences::default();
        PreferencePatch::SetLoopLockOverride(Some(true))
            .apply(&mut preferences)
            .expect("set lock");
        assert_eq!(preferences.loop_lock_override, Some(true));
        assert!(!preferences.loop_enabled);
        PreferencePatch::SetLoopLockOverride(Some(false))
            .apply(&mut preferences)
            .expect("cycle lock");
        assert_eq!(preferences.loop_lock_override, Some(false));
        assert!(!preferences.loop_enabled);
        PreferencePatch::SetLoopEnabled(true)
            .apply(&mut preferences)
            .expect("ordinary toggle");
        assert_eq!(preferences.loop_lock_override, None);
        assert!(preferences.loop_enabled);
        let payload = preferences.to_payload().expect("encode");
        assert_eq!(
            UiPreferences::from_payload(&payload).expect("decode"),
            preferences
        );
        let mut legacy = serde_json::to_value(&preferences).expect("encode legacy fixture");
        legacy
            .as_object_mut()
            .expect("object")
            .remove("loop_lock_override");
        let legacy = serde_json::to_string(&legacy).expect("legacy payload");
        assert_eq!(
            UiPreferences::from_payload(&legacy)
                .expect("decode without lock")
                .loop_lock_override,
            None
        );
    }

    #[test]
    fn user_shader_requires_exact_explicit_approval_and_round_trips_owner_state() {
        let original = "@fragment fn post_fragment() {}";
        let changed = "@fragment fn post_fragment() {}\n";
        let original_fingerprint =
            user_shader_content_fingerprint(original).expect("fingerprint source");
        let changed_fingerprint =
            user_shader_content_fingerprint(changed).expect("fingerprint whitespace edit");
        assert_ne!(original_fingerprint, changed_fingerprint);

        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_valid(UiPreferences::default());
        let initial = coordinator.snapshot();

        assert_eq!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetUserShaderEnabled(true),
            ),
            Err(PreferenceError::UserShaderNotImported)
        );
        assert_eq!(coordinator.snapshot(), initial);

        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::ImportUserShader {
                    source: original.to_owned(),
                },
            )
            .expect("import disables and clears approval");
        assert!(
            coordinator
                .snapshot()
                .enabled_user_shader_source()
                .is_none()
        );
        let imported = coordinator.snapshot();

        assert_eq!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(3),
                PreferencePatch::SetUserShaderEnabled(true),
            ),
            Err(PreferenceError::UserShaderNotApproved)
        );
        assert_eq!(coordinator.snapshot(), imported);

        assert_eq!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(4),
                PreferencePatch::ApproveUserShader {
                    expected_fingerprint: changed_fingerprint.clone(),
                },
            ),
            Err(PreferenceError::UserShaderApprovalMismatch)
        );
        assert_eq!(coordinator.snapshot(), imported);
        assert_eq!(
            coordinator.snapshot().enabled_user_shader_source(),
            None,
            "a mismatch cannot produce executable source"
        );

        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(5),
                PreferencePatch::ApproveUserShader {
                    expected_fingerprint: original_fingerprint.clone(),
                },
            )
            .expect("explicit matching approval");
        assert!(
            coordinator
                .snapshot()
                .enabled_user_shader_source()
                .is_none()
        );
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(7),
                PreferencePatch::SetUserShaderEnabled(true),
            )
            .expect("enable only after approval");
        assert_eq!(
            coordinator.snapshot().enabled_user_shader_source(),
            Some(original)
        );

        let payload = coordinator.snapshot().to_payload().expect("owner save");
        let restored = UiPreferences::from_payload(&payload).expect("owner reload");
        assert_eq!(restored, coordinator.snapshot());
        assert_eq!(restored.enabled_user_shader_source(), Some(original));
        let mut stale_receipt = serde_json::to_value(&restored).expect("encode stale receipt");
        stale_receipt["user_shader"]["source"] = format!("{original} ").into();
        assert_eq!(
            UiPreferences::from_payload(&stale_receipt.to_string()),
            Err(PreferenceError::InvalidStoredUserShaderApproval)
        );

        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(6),
                PreferencePatch::ImportUserShader {
                    source: changed.to_owned(),
                },
            )
            .expect("whitespace replacement clears stale approval");
        let replaced = coordinator.snapshot();
        assert_eq!(
            replaced.user_shader.as_ref().unwrap().approved_fingerprint,
            None
        );
        assert!(!replaced.user_shader.as_ref().unwrap().enabled);
        assert!(replaced.enabled_user_shader_source().is_none());

        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(8),
                PreferencePatch::ApproveUserShader {
                    expected_fingerprint: changed_fingerprint,
                },
            )
            .expect("new exact bytes need their own approval");
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(9),
                PreferencePatch::SetUserShaderEnabled(true),
            )
            .expect("enable new explicitly approved bytes");
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(10),
                PreferencePatch::RevokeUserShaderApproval,
            )
            .expect("revoke disables in the owner");
        let revoked = coordinator.snapshot();
        assert_eq!(
            revoked.user_shader.as_ref().unwrap().approved_fingerprint,
            None
        );
        assert!(!revoked.user_shader.as_ref().unwrap().enabled);
        assert!(revoked.enabled_user_shader_source().is_none());
        coordinator
            .apply_loaded_patch(PreferenceRequestId(11), PreferencePatch::ClearUserShader)
            .expect("clear source and receipt");
        assert_eq!(coordinator.snapshot().user_shader, None);
    }

    #[test]
    fn user_shader_bounds_malformed_receipts_and_aggregate_rejection_keep_last_good_state() {
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_valid(UiPreferences::default());
        let initial = coordinator.snapshot();
        assert_eq!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::ImportUserShader {
                    source: " \n\t".to_owned(),
                },
            ),
            Err(PreferenceError::UserShaderSourceEmpty)
        );
        assert_eq!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::ImportUserShader {
                    source: "x".repeat(MAX_USER_SHADER_SOURCE_BYTES + 1),
                },
            ),
            Err(PreferenceError::UserShaderSourceTooLarge)
        );
        assert_eq!(coordinator.snapshot(), initial);

        // Backslashes and newlines expand in JSON so a valid 64 KiB source can exceed
        // the complete 128 KiB profile payload limit. The rejected command must not
        // replace owner state or prevent the next ordinary command from succeeding.
        let escaped_source = "\\\n".repeat(MAX_USER_SHADER_SOURCE_BYTES / 2);
        assert_eq!(escaped_source.len(), MAX_USER_SHADER_SOURCE_BYTES);
        assert!(matches!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(3),
                PreferencePatch::ImportUserShader {
                    source: escaped_source,
                },
            ),
            Err(PreferenceError::PayloadTooLarge { .. })
        ));
        assert_eq!(coordinator.snapshot(), initial);
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(4),
                PreferencePatch::SetTextScalePercent(ENLARGED_TEXT_SCALE_PERCENT),
            )
            .expect("owner accepts next command after rejected import");

        let mut malformed = UiPreferences::default();
        malformed.user_shader = Some(UserShaderPreferences {
            source: "valid-looking source".into(),
            approved_fingerprint: Some("sha256:v1:not-a-digest".into()),
            enabled: false,
        });
        assert_eq!(
            malformed.clone().validate(),
            Err(PreferenceError::InvalidStoredUserShaderApproval)
        );
        malformed.user_shader.as_mut().unwrap().enabled = true;
        assert!(malformed.enabled_user_shader_source().is_none());

        let legacy = serde_json::to_value(UiPreferences::default()).expect("encode legacy");
        assert!(legacy.get("user_shader").is_none());
        let restored = UiPreferences::from_payload(&legacy.to_string()).expect("legacy defaults");
        assert_eq!(restored.user_shader, None);
    }

    #[test]
    fn ordinary_preference_command_cannot_overflow_approved_shader_profile() {
        let source = "\\".repeat(58 * 1024);
        let fingerprint = user_shader_content_fingerprint(&source).expect("fingerprint");
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_valid(UiPreferences::default());
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::ImportUserShader {
                    source: source.clone(),
                },
            )
            .expect("bounded source fits profile payload");
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::ApproveUserShader {
                    expected_fingerprint: fingerprint,
                },
            )
            .expect("explicit approval");
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(3),
                PreferencePatch::SetUserShaderEnabled(true),
            )
            .expect("enable approved shader");

        let last_good = coordinator.snapshot();
        let revision = coordinator.revision();
        let large_trash_path = PathBuf::from(format!("/{}", "p".repeat(14_000)));
        assert!(matches!(
            coordinator.apply_loaded_patch(
                PreferenceRequestId(4),
                PreferencePatch::SetTrashFolder(Some(large_trash_path)),
            ),
            Err(PreferenceError::PayloadTooLarge { .. })
        ));
        assert_eq!(coordinator.snapshot(), last_good);
        assert_eq!(coordinator.revision(), revision);
        assert_eq!(
            coordinator.snapshot().enabled_user_shader_source(),
            Some(source.as_str())
        );

        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(5),
                PreferencePatch::SetTextScalePercent(ENLARGED_TEXT_SCALE_PERCENT),
            )
            .expect("next non-overflowing command still commits");
        let restored = UiPreferences::from_payload(
            &coordinator.snapshot().to_payload().expect("last-good save"),
        )
        .expect("last-good source remains reloadable");
        assert_eq!(restored.enabled_user_shader_source(), Some(source.as_str()));
    }

    #[test]
    fn text_scale_defaults_for_old_payloads_and_persists_at_supported_sizes() {
        let mut payload = serde_json::to_value(UiPreferences::default()).expect("encode");
        payload
            .as_object_mut()
            .expect("object")
            .remove("text_scale_percent");
        let mut preferences = UiPreferences::from_payload(
            &serde_json::to_string(&payload).expect("encode old payload"),
        )
        .expect("decode old payload");
        assert_eq!(preferences.text_scale_percent, DEFAULT_TEXT_SCALE_PERCENT);

        assert!(
            PreferencePatch::SetTextScalePercent(ENLARGED_TEXT_SCALE_PERCENT)
                .apply(&mut preferences)
                .expect("set enlarged text")
        );
        assert_eq!(
            UiPreferences::from_payload(&preferences.to_payload().expect("persist"))
                .expect("restore")
                .text_scale_percent,
            ENLARGED_TEXT_SCALE_PERCENT
        );
        let before = preferences.clone();
        assert_eq!(
            PreferencePatch::SetTextScalePercent(150).apply(&mut preferences),
            Err(PreferenceError::InvalidTextScale)
        );
        assert_eq!(preferences, before);
        PreferencePatch::Reset
            .apply(&mut preferences)
            .expect("reset");
        assert_eq!(preferences.text_scale_percent, DEFAULT_TEXT_SCALE_PERCENT);
    }

    #[test]
    fn payload_omitting_output_volume_uses_full_volume_default() {
        let mut payload = serde_json::to_value(UiPreferences::default()).expect("encode");
        payload
            .as_object_mut()
            .expect("preferences object")
            .remove("output_volume");
        let payload = serde_json::to_string(&payload).expect("encode legacy payload");

        let preferences = UiPreferences::from_payload(&payload).expect("decode legacy payload");
        assert_eq!(preferences.output_volume, 1.0);
    }

    #[test]
    fn payload_omitting_audio_output_uses_empty_output_defaults() {
        let mut payload = serde_json::to_value(UiPreferences::default()).expect("encode");
        payload
            .as_object_mut()
            .expect("preferences object")
            .remove("audio_output");
        let payload = serde_json::to_string(&payload).expect("encode legacy payload");

        let preferences = UiPreferences::from_payload(&payload).expect("decode legacy payload");
        assert_eq!(preferences.audio_output, AudioOutputPreferences::default());
    }

    #[test]
    fn audio_output_round_trip_preserves_optional_fields() {
        let audio_output = AudioOutputPreferences {
            host: Some(String::from("CoreAudio")),
            device: Some(String::from("Built-in Output")),
            sample_rate: Some(48_000),
            buffer_size: Some(256),
        };
        let mut preferences = UiPreferences::default();
        preferences.audio_output = audio_output.clone();

        let payload = preferences.to_payload().expect("encode preferences");
        let restored = UiPreferences::from_payload(&payload).expect("decode preferences");
        assert_eq!(restored.audio_output, audio_output);
    }

    #[test]
    fn invalid_output_volume_patch_rejects_before_mutation() {
        for volume in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 1.01] {
            let mut preferences = UiPreferences::default();
            preferences.output_volume = 0.4;
            let before = preferences.clone();

            let error = PreferencePatch::SetOutputVolume(volume)
                .apply(&mut preferences)
                .expect_err("invalid volume must be rejected");
            assert_eq!(error, PreferenceError::InvalidOutputVolume);
            assert_eq!(preferences, before);
        }
    }

    #[test]
    fn invalid_audio_output_patch_rejects_before_mutation() {
        let invalid_outputs = [
            AudioOutputPreferences {
                host: Some(String::new()),
                ..AudioOutputPreferences::default()
            },
            AudioOutputPreferences {
                device: Some(String::from("USB\0Output")),
                ..AudioOutputPreferences::default()
            },
            AudioOutputPreferences {
                host: Some("h".repeat(MAX_AUDIO_OUTPUT_NAME_BYTES + 1)),
                ..AudioOutputPreferences::default()
            },
            AudioOutputPreferences {
                sample_rate: Some(0),
                ..AudioOutputPreferences::default()
            },
            AudioOutputPreferences {
                buffer_size: Some(0),
                ..AudioOutputPreferences::default()
            },
        ];

        for audio_output in invalid_outputs {
            let mut preferences = UiPreferences::default();
            preferences.theme.name = String::from("Keep this theme");
            preferences.output_volume = 0.4;
            let before = preferences.clone();

            let error = PreferencePatch::SetAudioOutput(audio_output)
                .apply(&mut preferences)
                .expect_err("invalid audio output must be rejected");
            assert!(matches!(
                error,
                PreferenceError::InvalidAudioOutputName { .. }
                    | PreferenceError::InvalidAudioOutputValue { .. }
            ));
            assert_eq!(preferences, before);
        }
    }

    #[test]
    fn audio_output_patch_preserves_other_preferences() {
        let mut preferences = UiPreferences::default();
        preferences.theme.name = String::from("Keep this theme");
        preferences.output_volume = 0.4;
        let theme = preferences.theme.clone();
        let audio_output = AudioOutputPreferences {
            host: Some(String::from("CoreAudio")),
            device: Some(String::from("USB Output")),
            sample_rate: Some(44_100),
            buffer_size: Some(512),
        };

        assert!(
            PreferencePatch::SetAudioOutput(audio_output.clone())
                .apply(&mut preferences)
                .expect("valid audio output patch")
        );
        assert_eq!(preferences.theme, theme);
        assert_eq!(preferences.output_volume, 0.4);
        assert_eq!(preferences.audio_output, audio_output);
    }

    #[test]
    fn output_volume_save_deadline_is_deterministic_without_sleeping() {
        let start = Instant::now();
        let mut deadline = PreferenceSaveDeadline::default();
        deadline.defer_output_volume(start);

        assert!(!deadline.permits_save(start + Duration::from_millis(249), false));
        assert!(deadline.permits_save(start + Duration::from_millis(250), false));
        assert!(deadline.permits_save(start, true));

        deadline.clear();
        assert!(deadline.permits_save(start, false));
    }

    #[test]
    fn pre_load_patches_preserve_saved_fields_and_apply_in_order() {
        let mut saved = UiPreferences::default();
        saved.theme.name = String::from("Saved");
        saved.theme.colors.accent = Color::rgb(4, 5, 6);
        saved.theme_source_path = Some(PathBuf::from("/tmp/saved.json"));
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.queue_patch(
            PreferenceRequestId(1),
            PreferencePatch::SetMotionEnabled(false),
        );
        coordinator.queue_patch(
            PreferenceRequestId(2),
            PreferencePatch::SetAutoplaySelection(false),
        );

        let outcomes = coordinator.load_valid(saved.clone());
        assert!(outcomes.iter().all(|outcome| outcome.result.is_ok()));
        let current = coordinator.snapshot();
        assert_eq!(current.theme.name, saved.theme.name);
        assert_eq!(current.theme.colors.accent, saved.theme.colors.accent);
        assert_eq!(current.theme_source_path, saved.theme_source_path);
        assert!(!current.theme.motion.enabled);
        assert!(!current.autoplay_selection);
        assert!(!current.loop_enabled);
    }

    #[test]
    fn invalid_load_rejects_normal_edits_but_reset_recovers() {
        let mut coordinator = PreferenceCoordinator::new();
        let failed = coordinator.load_invalid();
        assert!(failed.is_empty());
        let error = coordinator
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetLoopEnabled(true),
            )
            .expect_err("normal patch must fail");
        assert_eq!(error, PreferenceError::StoredPayloadInvalid);
        coordinator
            .apply_loaded_patch(PreferenceRequestId(2), PreferencePatch::Reset)
            .expect("reset invalid preferences");
        assert!(coordinator.valid());
        assert!(coordinator.is_dirty());
    }

    #[test]
    fn stale_save_ack_keeps_newer_preference_revision_dirty() {
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_missing();
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetMotionEnabled(false),
            )
            .expect("first patch");
        let first_revision = coordinator.revision();
        assert!(coordinator.accept_save_ack(first_revision));
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::SetLoopEnabled(true),
            )
            .expect("second patch");
        assert!(!coordinator.accept_save_ack(first_revision));
        assert!(coordinator.is_dirty());
    }

    #[test]
    fn reverting_before_save_ack_still_requires_latest_revision() {
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_missing();
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetMotionEnabled(false),
            )
            .expect("first patch");
        let first_revision = coordinator.revision();
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::SetMotionEnabled(true),
            )
            .expect("reverting patch");
        let latest_revision = coordinator.revision();
        assert!(coordinator.is_dirty());
        assert!(!coordinator.accept_save_ack(first_revision));
        assert!(coordinator.is_dirty());
        assert!(coordinator.accept_save_ack(latest_revision));
        assert!(!coordinator.is_dirty());
    }

    #[test]
    fn reverting_to_durable_baseline_before_save_queue_still_advances_revision() {
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_missing();
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(1),
                PreferencePatch::SetLoopEnabled(true),
            )
            .expect("first patch");
        coordinator
            .apply_loaded_patch(
                PreferenceRequestId(2),
                PreferencePatch::SetLoopEnabled(false),
            )
            .expect("reverting patch");
        assert_eq!(coordinator.snapshot(), UiPreferences::default());
        assert!(coordinator.is_dirty());
    }

    #[test]
    fn no_op_patch_still_produces_an_applied_outcome() {
        let mut coordinator = PreferenceCoordinator::new();
        coordinator.load_missing();
        coordinator.queue_patch(
            PreferenceRequestId(1),
            PreferencePatch::SetAutoplaySelection(true),
        );
        let outcomes = coordinator.load_valid(UiPreferences::default());
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok());
        assert_eq!(coordinator.revision().get(), 0);
    }

    #[test]
    fn payload_limit_is_checked_before_json_decode() {
        let payload = "x".repeat(UI_PREFERENCES_MAX_BYTES + 1);
        assert!(matches!(
            UiPreferences::from_payload(&payload),
            Err(PreferenceError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn help_tooltips_legacy_default_round_trip_and_reset() {
        let mut payload = serde_json::to_value(UiPreferences::default()).unwrap();
        payload
            .as_object_mut()
            .unwrap()
            .remove("help_tooltips_enabled");
        let mut preferences = UiPreferences::from_payload(&payload.to_string()).unwrap();
        assert!(preferences.help_tooltips_enabled);
        let original = preferences.clone();
        assert!(
            PreferencePatch::SetHelpTooltipsEnabled(false)
                .apply(&mut preferences)
                .unwrap()
        );
        assert!(
            !PreferencePatch::SetHelpTooltipsEnabled(false)
                .apply(&mut preferences)
                .unwrap()
        );
        let restored = UiPreferences::from_payload(&preferences.to_payload().unwrap()).unwrap();
        assert!(!restored.help_tooltips_enabled);
        assert_eq!(restored.theme, original.theme);
        assert_eq!(restored.audio_output, original.audio_output);
        PreferencePatch::Reset.apply(&mut preferences).unwrap();
        assert!(preferences.help_tooltips_enabled);
    }

    #[test]
    fn help_tooltips_preload_edits_preserve_order_and_other_saved_settings() {
        let mut saved = UiPreferences::default();
        saved.output_volume = 0.25;
        let mut coordinator = PreferenceCoordinator::new();
        for (id, enabled) in [(1, false), (2, true), (3, false)] {
            coordinator.queue_patch(
                PreferenceRequestId(id),
                PreferencePatch::SetHelpTooltipsEnabled(enabled),
            );
        }
        let outcomes = coordinator.load_valid(saved);
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(|outcome| outcome.result.is_ok()));
        let current = coordinator.snapshot();
        assert!(!current.help_tooltips_enabled);
        assert_eq!(current.output_volume, 0.25);
    }

    #[test]
    fn general_settings_legacy_defaults_and_persistence() {
        let mut payload = serde_json::to_value(UiPreferences::default()).unwrap();
        for field in ["rating_decay_weeks", "trash_folder"] {
            payload.as_object_mut().unwrap().remove(field);
        }
        let mut preferences = UiPreferences::from_payload(&payload.to_string()).unwrap();
        assert_eq!(preferences.rating_decay_weeks, 4);
        assert_eq!(preferences.trash_folder, None);
        PreferencePatch::SetRatingDecayWeeks(52)
            .apply(&mut preferences)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().to_path_buf();
        PreferencePatch::SetTrashFolder(Some(folder.clone()))
            .apply(&mut preferences)
            .unwrap();
        let restored = UiPreferences::from_payload(&preferences.to_payload().unwrap()).unwrap();
        assert_eq!(restored.rating_decay_weeks, 52);
        assert_eq!(restored.trash_folder, Some(folder));
        PreferencePatch::SetTrashFolder(None)
            .apply(&mut preferences)
            .unwrap();
        assert_eq!(preferences.trash_folder, None);
        PreferencePatch::Reset.apply(&mut preferences).unwrap();
        assert_eq!(preferences.rating_decay_weeks, 4);
    }

    #[test]
    fn invalid_general_settings_reject_before_mutation() {
        let mut preferences = UiPreferences::default();
        let original = preferences.clone();
        for weeks in [0, 53, u16::MAX] {
            assert_eq!(
                PreferencePatch::SetRatingDecayWeeks(weeks).apply(&mut preferences),
                Err(PreferenceError::InvalidRatingDecay)
            );
            assert_eq!(preferences, original);
        }
        let directory = tempfile::tempdir().unwrap();
        for path in [
            PathBuf::from("relative"),
            directory.path().join("../outside"),
            directory.path().join("bad\0name"),
        ] {
            assert_eq!(
                PreferencePatch::SetTrashFolder(Some(path)).apply(&mut preferences),
                Err(PreferenceError::InvalidTrashFolder)
            );
            assert_eq!(preferences, original);
        }
    }
}
