#![deny(missing_docs)]
#![deny(warnings)]

//! Pure tagged playback-mode policy for the sample browser.

use crate::{AudioFileId, PlaybackType};

/// Selects the loop mode for the currently loaded file without coupling it to a UI or library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackLoopPolicy {
    current_file: Option<AudioFileId>,
    playback_type: Option<PlaybackType>,
    enabled: bool,
    fallback: bool,
    manual_override: bool,
}

impl Default for PlaybackLoopPolicy {
    fn default() -> Self {
        Self::new(false)
    }
}

impl PlaybackLoopPolicy {
    /// Construct a policy with the supplied saved default.
    pub const fn new(fallback: bool) -> Self {
        Self {
            current_file: None,
            playback_type: None,
            enabled: fallback,
            fallback,
            manual_override: false,
        }
    }

    /// Return the effective loop mode for the next newly queued play.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Return the saved fallback used when an explicit preference projection changes an unknown
    /// or unloaded file's mode.
    pub const fn fallback(&self) -> bool {
        self.fallback
    }

    /// Return the currently prepared file identity, if any.
    pub fn current_file(&self) -> Option<&AudioFileId> {
        self.current_file.as_ref()
    }

    /// Return the current file's optional classified playback type.
    pub const fn playback_type(&self) -> Option<PlaybackType> {
        self.playback_type
    }

    /// Return whether the current file has a manual mode override.
    pub const fn has_manual_override(&self) -> bool {
        self.manual_override
    }

    /// Prepare a loaded file and its optional classification.
    ///
    /// A recognized classification controls a file unless that same file has a manual override.
    /// Unknown classifications retain the preceding effective mode, which lets an unclassified
    /// file inherit the current session mode without treating the saved fallback as a new event.
    pub fn prepare(&mut self, file: AudioFileId, playback_type: Option<PlaybackType>) {
        let same_file = self.current_file.as_ref() == Some(&file);
        if !same_file {
            self.manual_override = false;
        }
        self.current_file = Some(file);
        self.playback_type = playback_type;
        if !self.manual_override
            && let Some(playback_type) = playback_type
        {
            self.enabled = matches!(playback_type, PlaybackType::Loop);
        }
    }

    /// Set the effective mode from a manual choice.
    ///
    /// The choice is retained as a file-specific override when a file is currently prepared. A
    /// choice made before any file is loaded still changes the effective session mode, but does
    /// not create an override that can block a later recognized classification.
    pub fn set_manual(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.manual_override = self.current_file.is_some();
    }

    /// Apply a newly projected saved fallback without overwriting file-specific policy.
    ///
    /// A changed fallback reinitializes an unknown or unloaded file. Reapplying the same fallback
    /// leaves inherited mode untouched so an unrelated preference update cannot reset it.
    pub fn apply_preference(&mut self, fallback: bool) {
        let changed = self.fallback != fallback;
        self.fallback = fallback;
        if changed && !self.manual_override && self.playback_type.is_none() {
            self.enabled = fallback;
        }
    }

    /// Reset the file override and explicitly reapply the saved fallback or recognized tag.
    pub fn reset(&mut self, fallback: bool) {
        self.manual_override = false;
        self.fallback = fallback;
        self.enabled = self.playback_type.map_or(fallback, |playback_type| {
            matches!(playback_type, PlaybackType::Loop)
        });
    }

    /// Clear the current file and classification while retaining the effective mode and fallback.
    pub fn clear_file(&mut self) {
        self.current_file = None;
        self.playback_type = None;
        self.manual_override = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(name: &str) -> AudioFileId {
        crate::AudioFile {
            path: PathBuf::from("/source").join(name),
            relative_path: PathBuf::from(name),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
        .stable_id()
    }

    #[test]
    fn unknown_file_retains_the_preceding_effective_mode() {
        let mut policy = PlaybackLoopPolicy::new(false);
        policy.prepare(file("loop.wav"), Some(PlaybackType::Loop));
        assert!(policy.enabled());
        policy.prepare(file("unknown.wav"), None);
        assert!(policy.enabled());
        policy.prepare(file("one-shot.wav"), Some(PlaybackType::OneShot));
        assert!(!policy.enabled());
        policy.prepare(file("unknown-again.wav"), None);
        assert!(!policy.enabled());
    }

    #[test]
    fn same_file_manual_override_survives_reclassification() {
        let id = file("sample.wav");
        let mut policy = PlaybackLoopPolicy::new(false);
        policy.prepare(id.clone(), Some(PlaybackType::Loop));
        policy.set_manual(false);
        policy.prepare(id.clone(), Some(PlaybackType::OneShot));
        assert!(!policy.enabled());
        assert!(policy.has_manual_override());
        assert_eq!(policy.current_file(), Some(&id));
        policy.prepare(id.clone(), Some(PlaybackType::Loop));
        policy.apply_preference(true);
        assert!(!policy.enabled());
        assert!(policy.has_manual_override());
    }

    #[test]
    fn different_file_clears_override_and_tag_beats_fallback() {
        let mut policy = PlaybackLoopPolicy::new(true);
        policy.prepare(file("sample.wav"), Some(PlaybackType::OneShot));
        policy.set_manual(true);
        policy.prepare(file("loop.wav"), Some(PlaybackType::Loop));
        assert!(policy.enabled());
        assert!(!policy.has_manual_override());
        policy.apply_preference(false);
        assert!(policy.enabled());
    }

    #[test]
    fn preference_projection_preserves_unknown_inheritance_until_fallback_changes() {
        let mut policy = PlaybackLoopPolicy::new(false);
        policy.prepare(file("loop.wav"), Some(PlaybackType::Loop));
        policy.prepare(file("unknown.wav"), None);
        policy.apply_preference(false);
        assert!(policy.enabled());
        policy.apply_preference(true);
        assert!(policy.enabled());
        policy.apply_preference(false);
        assert!(!policy.enabled());
    }

    #[test]
    fn reset_reapplies_tag_or_fallback_and_clear_file_retains_mode() {
        let id = file("sample.wav");
        let mut policy = PlaybackLoopPolicy::new(false);
        policy.prepare(id.clone(), Some(PlaybackType::Loop));
        policy.set_manual(false);
        policy.reset(false);
        assert!(policy.enabled());
        assert!(!policy.has_manual_override());
        policy.clear_file();
        assert!(policy.enabled());
        assert_eq!(policy.fallback(), false);
        assert!(policy.current_file().is_none());
        assert!(policy.playback_type().is_none());

        policy.set_manual(true);
        assert!(policy.enabled());
        assert!(!policy.has_manual_override());
        policy.apply_preference(false);
        assert!(policy.enabled());
    }

    #[test]
    fn no_file_manual_choice_sets_effective_mode_without_override() {
        let mut policy = PlaybackLoopPolicy::new(false);
        policy.set_manual(true);
        assert!(policy.enabled());
        assert!(!policy.has_manual_override());
        policy.prepare(file("one-shot.wav"), Some(PlaybackType::OneShot));
        assert!(!policy.enabled());
    }
}
