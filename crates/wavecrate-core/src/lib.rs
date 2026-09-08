#![deny(missing_docs)]
#![deny(warnings)]

//! Pure browser data types and projection logic for the GPUI application.

use std::path::{Path, PathBuf};

mod curation;
mod folder_tree;
mod playback_history;
mod rating;
mod rating_filter;
mod selection;
mod waveform_region;
mod waveform_viewport;

pub use curation::{CurationAction, CurationTransition, plan_curation_action};
pub use folder_tree::{
    FolderScope, FolderTree, FolderTreeRow, FolderTreeState, folder_contains_relative_path,
};
pub use playback_history::{
    PLAYBACK_NAVIGATION_HISTORY_LIMIT, PlaybackHistoryEntry, PlaybackNavigationHistory,
    PlaybackSpan,
};
pub use rating::{MAX_RATING, MIN_RATING, RatingState, RatingTransition, next_rating_state};
pub use rating_filter::{RATING_FILTER_LEVELS, RatingFilter};
pub use selection::BrowserSelection;
pub use waveform_region::WaveformRegion;
pub use waveform_viewport::WaveformViewport;

/// Number of fixed collection slots supported by source curation metadata.
pub const COLLECTION_COUNT: usize = 6;

/// Bit mask containing every supported collection slot.
pub const COLLECTION_MASK: u8 = (1 << COLLECTION_COUNT) - 1;

/// Optional curation metadata attached to one browser audio file.
///
/// Scanned files may have no curation metadata until a catalog snapshot hydrates them. The
/// collection membership is deliberately a fixed bit mask so a projection never allocates one
/// collection object per row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFileCuration {
    rating: RatingState,
    collection_mask: u8,
    last_played_at: Option<i64>,
    last_curated_at: Option<i64>,
}

impl AudioFileCuration {
    /// Construct curation metadata when every rating and collection bit is valid.
    pub fn new(
        rating: RatingState,
        collection_mask: u8,
        last_played_at: Option<i64>,
        last_curated_at: Option<i64>,
    ) -> Option<Self> {
        (collection_mask & !COLLECTION_MASK == 0).then_some(Self {
            rating,
            collection_mask,
            last_played_at,
            last_curated_at,
        })
    }

    /// Construct curation metadata from the catalog's scalar rating fields.
    pub fn from_parts(
        rating: i8,
        locked: bool,
        collection_mask: u8,
        last_played_at: Option<i64>,
        last_curated_at: Option<i64>,
    ) -> Option<Self> {
        RatingState::new(rating, locked)
            .and_then(|rating| Self::new(rating, collection_mask, last_played_at, last_curated_at))
    }

    /// Return the validated rating and lock state.
    pub const fn rating(self) -> RatingState {
        self.rating
    }

    /// Return the fixed collection membership bit mask.
    pub const fn collection_mask(self) -> u8 {
        self.collection_mask
    }

    /// Return whether this file belongs to a valid collection slot.
    pub const fn has_collection(self, slot: usize) -> bool {
        slot < COLLECTION_COUNT && self.collection_mask & (1 << slot) != 0
    }

    /// Return the first collection slot for deterministic collection sorting.
    pub const fn first_collection(self) -> Option<usize> {
        if self.collection_mask == 0 {
            None
        } else {
            Some(self.collection_mask.trailing_zeros() as usize)
        }
    }

    /// Return the last-played timestamp, if the catalog supplied one.
    pub const fn last_played_at(self) -> Option<i64> {
        self.last_played_at
    }

    /// Return the last-curated timestamp, if the catalog supplied one.
    pub const fn last_curated_at(self) -> Option<i64> {
        self.last_curated_at
    }
}

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
    /// Optional catalog-owned curation metadata; direct filesystem scans leave this unset.
    pub curation: Option<AudioFileCuration>,
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
    /// Order by the signed curation rating.
    Rating,
    /// Order by the first assigned collection slot.
    Collection,
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
    collection_filter: Option<usize>,
    rating_filter: RatingFilter,
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
            collection_filter: None,
            rating_filter: RatingFilter::disabled(),
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

    /// Return the active source collection filter, if any.
    pub fn collection_filter(&self) -> Option<usize> {
        self.collection_filter
    }

    /// Return the active source rating filter, or a disabled filter when no level is selected.
    pub fn rating_filter(&self) -> RatingFilter {
        self.rating_filter
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

    /// Set the active collection filter while preserving query, folder, and sort state.
    ///
    /// Returns `false` for an invalid slot or when the requested filter is already active.
    pub fn set_collection_filter(
        &mut self,
        entries: &[AudioFile],
        collection: Option<usize>,
    ) -> bool {
        if collection.is_some_and(|slot| slot >= COLLECTION_COUNT)
            || self.collection_filter == collection
        {
            return false;
        }
        self.collection_filter = collection;
        self.rebuild(entries);
        true
    }

    /// Toggle one source collection filter and return whether the projection changed.
    pub fn toggle_collection_filter(&mut self, entries: &[AudioFile], slot: usize) -> bool {
        if slot >= COLLECTION_COUNT {
            return false;
        }
        let next = (self.collection_filter != Some(slot)).then_some(slot);
        self.set_collection_filter(entries, next)
    }

    /// Replace the rating filter while preserving query, folder, collection, and sort state.
    pub fn set_rating_filter(&mut self, entries: &[AudioFile], filter: RatingFilter) -> bool {
        if self.rating_filter == filter {
            return false;
        }
        self.rating_filter = filter;
        self.rebuild(entries);
        true
    }

    /// Toggle one rating filter level and return whether the projection changed.
    ///
    /// Invalid levels return `false` without changing the current projection.
    pub fn toggle_rating_filter(&mut self, entries: &[AudioFile], level: i8) -> bool {
        let Some(next) = self.rating_filter.toggled_level(level) else {
            return false;
        };
        self.set_rating_filter(entries, next)
    }

    /// Count each collection across the current name, folder, and rating scope.
    ///
    /// The active collection filter is intentionally ignored so sidebar counts remain useful
    /// while a different collection is selected. Callers should cache this result and refresh it
    /// when entries, query, folder, or rating scope changes rather than during row rendering.
    pub fn collection_counts(&self, entries: &[AudioFile]) -> [usize; COLLECTION_COUNT] {
        let query = self.query.trim().to_ascii_lowercase();
        let mut counts = [0; COLLECTION_COUNT];
        for entry in entries {
            if !self.entry_matches_scope(entry, &query) || !self.entry_matches_rating(entry) {
                continue;
            }
            let Some(curation) = entry.curation else {
                continue;
            };
            for (slot, count) in counts.iter_mut().enumerate() {
                if curation.has_collection(slot) {
                    *count += 1;
                }
            }
        }
        counts
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
                    && self.collection_filter.is_none_or(|slot| {
                        entry
                            .curation
                            .is_some_and(|curation| curation.has_collection(slot))
                    })
                    && self.entry_matches_rating(entry)
            })
            .map(|(entry_index, entry)| {
                (
                    entry_index,
                    entry.stable_id(),
                    file_name_sort_key(entry),
                    extension_sort_key(entry),
                    entry.byte_len,
                    entry.curation.map(|curation| curation.rating().value()),
                    entry.curation.and_then(AudioFileCuration::first_collection),
                )
            })
            .collect::<Vec<_>>();
        projected.sort_by(|left, right| {
            let ordering = match self.sort.column {
                SortColumn::Name => left.2.cmp(&right.2),
                SortColumn::Size => left.4.cmp(&right.4),
                SortColumn::Extension => left.3.cmp(&right.3),
                SortColumn::Rating => left.5.cmp(&right.5),
                SortColumn::Collection => left.6.cmp(&right.6),
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
            .map(|(entry_index, id, _, _, _, _, _)| BrowserRow { entry_index, id })
            .collect();
    }

    fn entry_matches_scope(&self, entry: &AudioFile, query: &str) -> bool {
        self.folder.as_deref().is_none_or(|folder| {
            folder_contains_relative_path(folder, &entry.relative_path, self.folder_scope)
        }) && audio_file_matches_name_query(entry, query)
    }

    fn entry_matches_rating(&self, entry: &AudioFile) -> bool {
        self.rating_filter.is_disabled()
            || entry
                .curation
                .is_some_and(|curation| self.rating_filter.matches(curation.rating()))
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
            curation: None,
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
    fn collection_filter_intersects_name_and_folder_scope_and_restores_focus() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/keep.wav"),
                relative_path: PathBuf::from("kit/keep.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(2, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/other.wav"),
                relative_path: PathBuf::from("kit/other.wav"),
                byte_len: 2,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/elsewhere/keep.wav"),
                relative_path: PathBuf::from("elsewhere/keep.wav"),
                byte_len: 3,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(1, false, 0b0000_0001, None, None),
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        model.set_query(&entries, "wav");
        let keep_id = entries[0].stable_id();
        assert!(model.set_collection_filter(&entries, Some(0)));
        assert_eq!(model.collection_filter(), Some(0));
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].id, keep_id);
        assert_eq!(model.collection_counts(&entries), [1, 1, 0, 0, 0, 0]);
        assert_eq!(model.restore_focus(Some(&keep_id), Some(0)), Some(0));

        assert!(model.set_collection_filter(&entries, None));
        assert_eq!(model.sort(), SortSpec::default());
        assert_eq!(model.query(), "wav");
        assert_eq!(model.folder(), Some(Path::new("kit")));
        assert!(!model.set_collection_filter(&entries, Some(COLLECTION_COUNT)));
    }

    #[test]
    fn rating_filter_distinguishes_locked_keep_and_excludes_unknown_metadata() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/t3.wav"),
                relative_path: PathBuf::from("t3.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-3, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/k3.wav"),
                relative_path: PathBuf::from("k3.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(3, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/k4.wav"),
                relative_path: PathBuf::from("k4.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(3, true, 0, None, None),
            },
            model_entry("/tmp/unknown.wav", 1),
        ];
        let mut model = BrowserViewModel::new(&entries);
        assert_eq!(model.rows().len(), entries.len());

        assert!(model.toggle_rating_filter(&entries, 3));
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 1);
        assert!(model.rating_filter().contains_level(3));

        assert!(model.toggle_rating_filter(&entries, 4));
        assert_eq!(model.rows().len(), 2);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(!model.rating_filter().contains_level(0));
    }

    #[test]
    fn rating_filter_intersects_name_folder_collection_and_collection_counts() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/kick-a.wav"),
                relative_path: PathBuf::from("kit/kick-a.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/kick-b.wav"),
                relative_path: PathBuf::from("kit/kick-b.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/elsewhere/kick-c.wav"),
                relative_path: PathBuf::from("elsewhere/kick-c.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/pad.wav"),
                relative_path: PathBuf::from("kit/pad.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(1, false, 0b0000_0001, None, None),
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        model.set_query(&entries, "kick");
        assert!(model.set_collection_filter(&entries, Some(0)));
        assert!(model.toggle_rating_filter(&entries, -1));

        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 0);
        assert_eq!(model.collection_counts(&entries), [1, 1, 0, 0, 0, 0]);

        assert!(model.toggle_rating_filter(&entries, -1));
        assert_eq!(model.rows().len(), 1);
        assert!(model.rating_filter().is_disabled());
        assert_eq!(model.rows()[0].entry_index, 0);
    }

    #[test]
    fn rating_filter_preserves_focus_position_when_focused_row_disappears() {
        let mut entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/alpha.wav"),
                relative_path: PathBuf::from("alpha.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(0, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/bravo.wav"),
                relative_path: PathBuf::from("bravo.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(0, false, 0, None, None),
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        let alpha_id = entries[0].stable_id();
        assert!(model.toggle_rating_filter(&entries, 0));
        assert_eq!(model.rows().len(), 2);

        entries[0].curation = AudioFileCuration::from_parts(1, false, 0, None, None);
        model.set_entries(&entries);
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 1);
        assert_eq!(model.restore_focus(Some(&alpha_id), Some(0)), Some(1));
    }

    #[test]
    fn rating_and_collection_sort_use_curation_values_then_stable_name() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/zulu.wav"),
                relative_path: PathBuf::from("zulu.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/alpha.wav"),
                relative_path: PathBuf::from("alpha.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/bravo.wav"),
                relative_path: PathBuf::from("bravo.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                curation: None,
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.toggle_sort(&entries, SortColumn::Rating);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        model.toggle_sort(&entries, SortColumn::Collection);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        model.toggle_sort(&entries, SortColumn::Collection);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
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
                curation: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/bravo.wav"),
                relative_path: PathBuf::from("kit/bravo.wav"),
                byte_len: 10,
                duration_seconds: None,
                sample_rate: None,
                curation: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/charlie.wav"),
                relative_path: PathBuf::from("kit/charlie.wav"),
                byte_len: 20,
                duration_seconds: None,
                sample_rate: None,
                curation: None,
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

    #[test]
    fn curation_metadata_rejects_unknown_slots_and_exposes_compact_state() {
        let curation = AudioFileCuration::from_parts(3, true, 0b0010_0101, Some(11), Some(22))
            .expect("valid curation metadata");
        assert_eq!(curation.rating(), RatingState::new(3, true).unwrap());
        assert_eq!(curation.collection_mask(), 0b0010_0101);
        assert!(curation.has_collection(0));
        assert!(curation.has_collection(2));
        assert!(curation.has_collection(5));
        assert_eq!(curation.first_collection(), Some(0));
        assert_eq!(curation.last_played_at(), Some(11));
        assert_eq!(curation.last_curated_at(), Some(22));
        assert!(AudioFileCuration::from_parts(0, false, 0b0100_0000, None, None).is_none());
    }
}
