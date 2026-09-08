use std::path::{Path, PathBuf};

/// Maximum number of playback locations retained by [`PlaybackNavigationHistory`].
pub const PLAYBACK_NAVIGATION_HISTORY_LIMIT: usize = 50;

/// An ordered, finite normalized playback span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackSpan {
    start: f32,
    end: f32,
}

impl PlaybackSpan {
    /// Construct a span by replacing non-finite values with `0.0`, clamping both values to
    /// `0.0..=1.0`, and ordering the resulting endpoints.
    pub fn new(start: f32, end: f32) -> Self {
        let start = normalize_playback_ratio(start);
        let end = normalize_playback_ratio(end);
        if end < start {
            Self {
                start: end,
                end: start,
            }
        } else {
            Self { start, end }
        }
    }

    /// Return the normalized start ratio.
    pub fn start(self) -> f32 {
        self.start
    }

    /// Return the normalized end ratio.
    pub fn end(self) -> f32 {
        self.end
    }
}

/// One file and normalized span in playback navigation history.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackHistoryEntry {
    path: PathBuf,
    span: PlaybackSpan,
}

impl PlaybackHistoryEntry {
    /// Construct a history entry with a normalized, ordered playback span.
    pub fn new(path: impl Into<PathBuf>, start: f32, end: f32) -> Self {
        Self {
            path: path.into(),
            span: PlaybackSpan::new(start, end),
        }
    }

    /// Return the original path, preserving platform-native path bytes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the normalized playback span.
    pub fn span(&self) -> PlaybackSpan {
        self.span
    }
}

/// Bounded back/forward history for intentional playback locations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackNavigationHistory {
    entries: Vec<PlaybackHistoryEntry>,
    cursor: Option<usize>,
}

impl PlaybackNavigationHistory {
    /// Return the number of retained history entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the history has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return whether a previous entry is available.
    pub fn can_back(&self) -> bool {
        self.cursor.is_some_and(|cursor| cursor > 0)
    }

    /// Return whether a later entry is available.
    pub fn can_forward(&self) -> bool {
        self.cursor
            .is_some_and(|cursor| cursor + 1 < self.entries.len())
    }

    /// Record a playback location, truncating forward history after a back navigation.
    ///
    /// Consecutive entries with the same path and normalized span are collapsed into one entry.
    pub fn record(&mut self, path: impl Into<PathBuf>, start: f32, end: f32) {
        let entry = PlaybackHistoryEntry::new(path, start, end);
        if let Some(cursor) = self.cursor
            && cursor + 1 < self.entries.len()
        {
            self.entries.truncate(cursor + 1);
        }
        if self.entries.last() == Some(&entry) {
            self.cursor = self.entries.len().checked_sub(1);
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > PLAYBACK_NAVIGATION_HISTORY_LIMIT {
            let overflow = self.entries.len() - PLAYBACK_NAVIGATION_HISTORY_LIMIT;
            self.entries.drain(0..overflow);
        }
        self.cursor = self.entries.len().checked_sub(1);
    }

    /// Move to and return the previous playback location, if one exists.
    pub fn back(&mut self) -> Option<PlaybackHistoryEntry> {
        if self.entries.len() < 2 {
            return None;
        }
        let current = self.cursor.unwrap_or(self.entries.len() - 1);
        if current == 0 {
            self.cursor = Some(0);
            return None;
        }
        let previous = current - 1;
        self.cursor = Some(previous);
        self.entries.get(previous).cloned()
    }

    /// Move to and return the next playback location, if one exists.
    pub fn forward(&mut self) -> Option<PlaybackHistoryEntry> {
        let current = self.cursor?;
        let next = current + 1;
        if next >= self.entries.len() {
            return None;
        }
        self.cursor = Some(next);
        self.entries.get(next).cloned()
    }
}

fn normalize_playback_ratio(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{PLAYBACK_NAVIGATION_HISTORY_LIMIT, PlaybackNavigationHistory, PlaybackSpan};

    #[test]
    fn history_steps_back_and_forward_without_recording_replays() {
        let mut history = PlaybackNavigationHistory::default();
        history.record("kick.wav", 0.0, 1.0);
        history.record("snare.wav", 0.2, 0.5);

        assert!(history.can_back());
        assert!(!history.can_forward());
        assert_eq!(
            history.back().as_ref().map(|entry| entry.path()),
            Some(std::path::Path::new("kick.wav"))
        );
        assert!(!history.can_back());
        assert!(history.can_forward());
        assert_eq!(
            history.forward().as_ref().map(|entry| entry.path()),
            Some(std::path::Path::new("snare.wav"))
        );
        assert!(!history.can_forward());
    }

    #[test]
    fn recording_after_back_discards_forward_history() {
        let mut history = PlaybackNavigationHistory::default();
        history.record("a.wav", 0.0, 1.0);
        history.record("b.wav", 0.0, 1.0);
        history.record("c.wav", 0.0, 1.0);

        assert_eq!(
            history.back().as_ref().map(|entry| entry.path()),
            Some(std::path::Path::new("b.wav"))
        );
        history.record("d.wav", 0.25, 0.75);

        assert!(!history.can_forward());
        assert_eq!(
            history.back().as_ref().map(|entry| entry.path()),
            Some(std::path::Path::new("b.wav"))
        );
    }

    #[test]
    fn history_keeps_last_fifty_entries() {
        let mut history = PlaybackNavigationHistory::default();
        for index in 0..(PLAYBACK_NAVIGATION_HISTORY_LIMIT + 5) {
            history.record(format!("sample-{index}.wav"), 0.0, 1.0);
        }

        assert_eq!(history.len(), PLAYBACK_NAVIGATION_HISTORY_LIMIT);
        for _ in 0..PLAYBACK_NAVIGATION_HISTORY_LIMIT - 1 {
            let _ = history.back();
        }
        assert!(!history.can_back());
        assert_eq!(
            history.forward().as_ref().map(|entry| entry.path()),
            Some(std::path::Path::new("sample-6.wav"))
        );
    }

    #[test]
    fn duplicate_contiguous_entries_are_collapsed_but_changed_span_is_kept() {
        let mut history = PlaybackNavigationHistory::default();
        history.record("loop.wav", 0.9, 0.1);
        history.record("loop.wav", 0.9, 0.1);
        assert_eq!(history.len(), 1);
        history.record("loop.wav", 0.2, 0.8);
        assert_eq!(history.len(), 2);
        assert_eq!(
            history.back().expect("first span").span(),
            PlaybackSpan::new(0.1, 0.9)
        );
    }

    #[test]
    fn span_normalization_is_finite_clamped_and_ordered() {
        assert_eq!(PlaybackSpan::new(1.5, -0.25), PlaybackSpan::new(0.0, 1.0));
        assert_eq!(
            PlaybackSpan::new(f32::NAN, f32::INFINITY),
            PlaybackSpan::new(0.0, 0.0)
        );
        assert_eq!(
            PlaybackSpan::new(f32::NEG_INFINITY, 0.75),
            PlaybackSpan::new(0.0, 0.75)
        );
    }

    #[test]
    fn empty_and_boundary_navigation_are_no_ops() {
        let mut history = PlaybackNavigationHistory::default();
        assert!(history.is_empty());
        assert!(!history.can_back());
        assert!(!history.can_forward());
        assert!(history.back().is_none());
        assert!(history.forward().is_none());

        history.record("only.wav", 0.0, 1.0);
        assert!(history.back().is_none());
        assert!(history.forward().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn history_preserves_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let raw_path = PathBuf::from(OsString::from_vec(vec![b'k', 0x80, b'.', b'w', b'a', b'v']));
        let mut history = PlaybackNavigationHistory::default();
        history.record(raw_path.clone(), 0.0, 1.0);
        history.record("other.wav", 0.0, 1.0);

        assert_eq!(history.back().expect("non-UTF-8 path").path(), raw_path);
    }
}
