//! GPUI-free persisted preferences and their ordered session patches.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use wavecrate_design::{Theme, ThemeError, validate_theme};
use wavecrate_library::sample_sources::UI_PREFERENCES_MAX_BYTES;

use crate::SessionError;

/// The only persisted GPUI-preferences document version understood by this session owner.
pub const UI_PREFERENCES_SCHEMA_VERSION: u32 = 1;
const MAX_THEME_SOURCE_PATH_BYTES: usize = 16 * 1024;

fn default_autoplay_selection() -> bool {
    true
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
    /// Whether audition playback loops its active region.
    #[serde(default)]
    pub loop_enabled: bool,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            schema_version: UI_PREFERENCES_SCHEMA_VERSION,
            theme: Theme::default(),
            theme_source_path: None,
            autoplay_selection: true,
            loop_enabled: false,
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
    /// Change the audition loop preference.
    SetLoopEnabled(bool),
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
            Self::SetLoopEnabled(enabled) => preferences.loop_enabled = enabled,
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
                let result = self.apply_patch(edit.request, edit.patch);
                PreferenceOutcome {
                    request: edit.request,
                    result,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecrate_design::Color;

    #[test]
    fn payload_round_trip_validates_full_theme_and_path() {
        let mut preferences = UiPreferences::default();
        preferences.theme.name = String::from("Saved");
        preferences.theme.colors.accent = Color::rgb(1, 2, 3);
        preferences.theme_source_path = Some(PathBuf::from("/tmp/custom theme.json"));
        preferences.autoplay_selection = false;
        preferences.loop_enabled = true;

        let payload = preferences.to_payload().expect("encode preferences");
        assert_eq!(
            UiPreferences::from_payload(&payload).expect("decode"),
            preferences
        );
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
}
