#![deny(missing_docs)]
#![deny(warnings)]

//! Pure browser data types and projection logic for the GPUI application.

use std::path::{Path, PathBuf};

mod folder_tree;
mod playback_history;
mod selection;
mod waveform_region;
mod waveform_viewport;

pub use folder_tree::{
    FolderScope, FolderTree, FolderTreeRow, FolderTreeState, folder_contains_relative_path,
};
pub use playback_history::{
    PLAYBACK_NAVIGATION_HISTORY_LIMIT, PlaybackHistoryEntry, PlaybackNavigationHistory,
    PlaybackSpan,
};
pub use selection::BrowserSelection;
pub use waveform_region::WaveformRegion;
pub use waveform_viewport::WaveformViewport;

/// A source audio file discovered by a folder scan.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFile {
    /// Absolute or user-provided path to the audio file.
    pub path: PathBuf,
    /// Path relative to the selected folder.
    pub relative_path: PathBuf,
    /// File size in bytes when metadata was available.
    pub byte_len: u64,
    /// Duration reported by a container-level metadata probe.
    pub duration_seconds: Option<f32>,
    /// Sample rate reported by a container-level metadata probe.
    pub sample_rate: Option<u32>,
}

/// Stable identity for a discovered file in the read-only browser.
///
/// The standalone browser has no durable database identifier yet, so scans supply canonical paths
/// and synthetic or unavailable entries retain their supplied path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AudioFileId(PathBuf);

impl AudioFileId {
    /// Return the path used as this identity.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl AudioFile {
    /// Return the stable identity used to preserve focus across projection changes.
    pub fn stable_id(&self) -> AudioFileId {
        AudioFileId(self.path.clone())
    }
}

/// Browser column used to order the visible rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortColumn {
    /// Order by the filename.
    #[default]
    Name,
    /// Order by the file size in bytes.
    Size,
    /// Order by the lowercased filename extension.
    Extension,
}

/// Direction used by a browser sort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortDirection {
    /// Lowest or alphabetically first values first.
    #[default]
    Ascending,
    /// Highest or alphabetically last values first.
    Descending,
}

impl SortDirection {
    fn toggled(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// Current browser ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortSpec {
    /// Column used for the primary comparison.
    pub column: SortColumn,
    /// Direction applied to the primary and deterministic tie comparisons.
    pub direction: SortDirection,
}

/// A visible row in the browser projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserRow {
    /// Index into the unfiltered scan result.
    pub entry_index: usize,
    /// Stable identity of the entry at [`BrowserRow::entry_index`].
    pub id: AudioFileId,
}

/// Read-only browser view model for filtering, sorting, and keyboard navigation.
///
/// It projects stable file identities back to indices in the current scan. The projection owns
/// no persistence and does not mutate the source folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserViewModel {
    query: String,
    sort: SortSpec,
    folder: Option<PathBuf>,
    folder_scope: FolderScope,
    rows: Vec<BrowserRow>,
}

impl BrowserViewModel {
    /// Build the default name-ascending projection for a scan.
    pub fn new(entries: &[AudioFile]) -> Self {
        let mut model = Self {
            query: String::new(),
            sort: SortSpec::default(),
            folder: None,
            folder_scope: FolderScope::DirectOnly,
            rows: Vec::new(),
        };
        model.rebuild(entries);
        model
    }

    /// Return the untrimmed text currently shown in the search field.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Return the current ordering.
    pub fn sort(&self) -> SortSpec {
        self.sort
    }

    /// Return the selected relative folder, if folder filtering is active.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// Return the direct or recursive folder filtering mode.
    pub fn folder_scope(&self) -> FolderScope {
        self.folder_scope
    }

    /// Return visible rows in display order.
    pub fn rows(&self) -> &[BrowserRow] {
        &self.rows
    }

    /// Replace the search query and rebuild visible rows.
    pub fn set_query(&mut self, entries: &[AudioFile], query: impl Into<String>) {
        self.query = query.into();
        self.rebuild(entries);
    }

    /// Replace the scan while preserving the current query and sort choice.
    pub fn set_entries(&mut self, entries: &[AudioFile]) {
        self.rebuild(entries);
    }

    /// Set the selected folder and whether descendants are included, preserving search and sort.
    pub fn set_folder_scope(
        &mut self,
        entries: &[AudioFile],
        folder: Option<PathBuf>,
        scope: FolderScope,
    ) {
        self.folder = folder;
        self.folder_scope = scope;
        self.rebuild(entries);
    }

    /// Toggle a column, or start a new column in ascending order.
    pub fn toggle_sort(&mut self, entries: &[AudioFile], column: SortColumn) {
        if self.sort.column == column {
            self.sort.direction = self.sort.direction.toggled();
        } else {
            self.sort = SortSpec {
                column,
                direction: SortDirection::Ascending,
            };
        }
        self.rebuild(entries);
    }

    /// Restore a focused identity after a filter or sort change.
    ///
    /// A still-visible identity wins. If it was filtered out, the row nearest its previous
    /// visible position is selected, which keeps keyboard focus deterministic as the result set
    /// shrinks.
    pub fn restore_focus(
        &self,
        focused_id: Option<&AudioFileId>,
        previous_visible_position: Option<usize>,
    ) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        if let Some(focused_id) = focused_id
            && let Some(row) = self.rows.iter().find(|row| &row.id == focused_id)
        {
            return Some(row.entry_index);
        }
        let position = previous_visible_position
            .unwrap_or_default()
            .min(self.rows.len().saturating_sub(1));
        Some(self.rows[position].entry_index)
    }

    /// Return the visible position of an entry by stable identity.
    pub fn visible_position_for(&self, entry_id: &AudioFileId) -> Option<usize> {
        self.rows.iter().position(|row| &row.id == entry_id)
    }

    fn rebuild(&mut self, entries: &[AudioFile]) {
        let query = self.query.trim().to_ascii_lowercase();
        let mut projected = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                self.folder.as_deref().is_none_or(|folder| {
                    folder_contains_relative_path(folder, &entry.relative_path, self.folder_scope)
                }) && audio_file_matches_name_query(entry, &query)
            })
            .map(|(entry_index, entry)| {
                (
                    entry_index,
                    entry.stable_id(),
                    file_name_sort_key(entry),
                    extension_sort_key(entry),
                    entry.byte_len,
                )
            })
            .collect::<Vec<_>>();
        projected.sort_by(|left, right| {
            let ordering = match self.sort.column {
                SortColumn::Name => left.2.cmp(&right.2),
                SortColumn::Size => left.4.cmp(&right.4),
                SortColumn::Extension => left.3.cmp(&right.3),
            }
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.1.path().cmp(right.1.path()));
            match self.sort.direction {
                SortDirection::Ascending => ordering,
                SortDirection::Descending => ordering.reverse(),
            }
        });
        self.rows = projected
            .into_iter()
            .map(|(entry_index, id, _, _, _)| BrowserRow { entry_index, id })
            .collect();
    }
}

fn audio_file_matches_name_query(entry: &AudioFile, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let file_name = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let stem = entry
        .path
        .file_stem()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    file_name.contains(query) || stem.contains(query)
}

fn file_name_sort_key(entry: &AudioFile) -> String {
    entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| entry.relative_path.to_string_lossy().to_ascii_lowercase())
}

fn extension_sort_key(entry: &AudioFile) -> String {
    entry
        .path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Format a duration for compact display in the browser chrome.
pub fn format_duration(duration_seconds: Option<f32>) -> String {
    let Some(duration_seconds) = duration_seconds.filter(|duration| duration.is_finite()) else {
        return "--:--".to_string();
    };
    let total_seconds = duration_seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_entry(path: &str, byte_len: u64) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(path),
            byte_len,
            duration_seconds: None,
            sample_rate: None,
        }
    }

    #[test]
    fn browser_name_filter_matches_filename_or_stem_with_ascii_case_folding() {
        let entries = vec![
            model_entry("/tmp/Deep Kick.WAV", 10),
            model_entry("/tmp/Über Pad.aiff", 20),
            model_entry("/tmp/ambient.wav", 30),
        ];
        let mut model = BrowserViewModel::new(&entries);

        model.set_query(&entries, "  KICK  ");
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 0);

        model.set_query(&entries, " ÜB ");
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 1);

        model.set_query(&entries, "  .wav ");
        assert_eq!(model.rows().len(), 2);
        assert_eq!(model.rows()[0].entry_index, 2);
        assert_eq!(model.rows()[1].entry_index, 0);
    }

    #[test]
    fn browser_projection_preserves_identity_and_falls_back_to_previous_position() {
        let entries = vec![
            model_entry("/tmp/alpha.wav", 30),
            model_entry("/tmp/bravo.wav", 20),
            model_entry("/tmp/charlie.wav", 10),
        ];
        let mut model = BrowserViewModel::new(&entries);
        let bravo_id = entries[1].stable_id();

        model.set_query(&entries, "wav");
        assert_eq!(model.restore_focus(Some(&bravo_id), Some(1)), Some(1));

        model.set_query(&entries, "char");
        assert_eq!(model.restore_focus(Some(&bravo_id), Some(1)), Some(2));
    }

    #[test]
    fn browser_sort_toggles_same_column_and_starts_new_columns_ascending() {
        let entries = vec![
            model_entry("/tmp/zeta.wav", 10),
            model_entry("/tmp/alpha.aiff", 30),
            model_entry("/tmp/bravo.wav", 20),
        ];
        let mut model = BrowserViewModel::new(&entries);
        assert_eq!(model.rows()[0].entry_index, 1);

        model.toggle_sort(&entries, SortColumn::Size);
        assert_eq!(
            model.sort(),
            SortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Ascending,
            }
        );
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0, 2, 1]
        );

        model.toggle_sort(&entries, SortColumn::Size);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn browser_folder_scope_combines_with_name_filter_and_sort() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/source/kitchen/alpha.wav"),
                relative_path: PathBuf::from("kitchen/alpha.wav"),
                byte_len: 30,
                duration_seconds: None,
                sample_rate: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/bravo.wav"),
                relative_path: PathBuf::from("kit/bravo.wav"),
                byte_len: 10,
                duration_seconds: None,
                sample_rate: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/charlie.wav"),
                relative_path: PathBuf::from("kit/charlie.wav"),
                byte_len: 20,
                duration_seconds: None,
                sample_rate: None,
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        model.set_query(&entries, "wav");
        model.toggle_sort(&entries, SortColumn::Size);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from(".")),
            FolderScope::AllDescendants,
        );
        assert_eq!(model.rows().len(), 3);
    }

    #[test]
    fn duration_format_is_stable_for_missing_and_long_values() {
        assert_eq!(format_duration(None), "--:--");
        assert_eq!(format_duration(Some(65.4)), "01:05");
        assert_eq!(format_duration(Some(3600.0)), "60:00");
    }
}
