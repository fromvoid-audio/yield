//! GPUI-free persisted preferences and their ordered session patches.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use yield_design::{Theme, ThemeError, validate_theme};
use yield_library::sample_sources::UI_PREFERENCES_MAX_BYTES;

use crate::SessionError;

/// The only persisted GPUI-preferences document version understood by this session owner.
pub const UI_PREFERENCES_SCHEMA_VERSION: u32 = 1;
const MAX_THEME_SOURCE_PATH_BYTES: usize = 16 * 1024;
const MAX_AUDIO_OUTPUT_NAME_BYTES: usize = 1024;
const OUTPUT_VOLUME_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Default interval between positive-rating decay steps, in weeks.
pub const DEFAULT_RATING_DECAY_WEEKS: u16 = 4;
/// Shortest supported rating-decay interval.
pub const MIN_RATING_DECAY_WEEKS: u16 = 1;
/// Longest supported rating-decay interval.
pub const MAX_RATING_DECAY_WEEKS: u16 = 52;

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
    /// Whether audition playback uses the raw peak of its active span to normalize gain.
    #[serde(default)]
    pub normalized_audition_enabled: bool,
    /// Linear output volume applied to audition playback.
    #[serde(default = "default_output_volume")]
    pub output_volume: f32,
    /// Optional output routing requested by the user.
    #[serde(default)]
    pub audio_output: AudioOutputPreferences,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            schema_version: UI_PREFERENCES_SCHEMA_VERSION,
            theme: Theme::default(),
            theme_source_path: None,
            autoplay_selection: true,
            help_tooltips_enabled: true,
            rating_decay_weeks: DEFAULT_RATING_DECAY_WEEKS,
            trash_folder: None,
            loop_enabled: false,
            normalized_audition_enabled: false,
            output_volume: 1.0,
            audio_output: AudioOutputPreferences::default(),
        }
    }
}

impl UiPreferences {
    /// Validate the complete preference document without performing filesystem I/O.
    pub fn validate(self) -> Result<Self, PreferenceError> {
        if self.schema_version != UI_PREFERENCES_SCHEMA_VERSION {
            return Err(PreferenceError::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        let theme = validate_theme(self.theme)
            .map_err(|error| PreferenceError::InvalidTheme(error.to_string()))?;
        if let Some(path) = self.theme_source_path.as_deref() {
            validate_theme_source_path(path)?;
        }
        validate_rating_decay_weeks(self.rating_decay_weeks)?;
        validate_trash_folder(self.trash_folder.as_deref())?;
        validate_output_volume(self.output_volume)?;
        self.audio_output.validate()?;
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
    /// Change the peak-normalized audition preference.
    SetNormalizedAuditionEnabled(bool),
    /// Change the linear audition output volume.
    SetOutputVolume(f32),
    /// Replace all optional audio output routing fields atomically.
    SetAudioOutput(AudioOutputPreferences),
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
            Self::SetLoopEnabled(enabled) => preferences.loop_enabled = enabled,
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
            Self::Reset => *preferences = Self::default_preferences(),
        }
        Ok(before != *preferences)
    }

    fn is_reset(&self) -> bool {
        matches!(self, Self::Reset)
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
        let changed = patch.apply(&mut self.current)?;
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
        preferences.normalized_audition_enabled = true;
        preferences.output_volume = 0.37;

        let payload = preferences.to_payload().expect("encode preferences");
        assert_eq!(
            UiPreferences::from_payload(&payload).expect("decode"),
            preferences
        );
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
