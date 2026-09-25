#![deny(missing_docs)]
#![deny(warnings)]

//! Pure browser data types and projection logic for the GPUI application.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod bpm_snap;
mod curation;
mod curation_queue;
mod folder_tree;
mod last_played;
mod playback_history;
mod playback_loop_policy;
mod playback_type_filter;
mod played_range_history;
mod playmark_timing;
mod random_audition;
mod random_navigation;
mod rating;
mod rating_filter;
mod sample_name;
mod selection;
mod tag_category;
mod tag_filter;
mod tag_input;
mod waveform_edit_effects;
mod waveform_edit_fade_drag;
mod waveform_edit_region;
mod waveform_region;
mod waveform_viewport;

pub use bpm_snap::snap_resize_ratio_to_whole_bpm;
pub use curation::{CurationAction, CurationTransition, plan_curation_action};
pub use curation_queue::{
    CurationQueueDecision, CurationQueueKey, CurationQueueMode, CurationQueuePriority,
    CurationQueueScope, CurationTagFacts, curation_queue_decision,
};
pub use folder_tree::{
    FolderScope, FolderTree, FolderTreeRow, FolderTreeState, folder_contains_relative_path,
};
pub use last_played::{
    SECONDS_PER_DAY, format_last_played, last_played_age_seconds, last_played_sort_key,
};
pub use playback_history::{
    PLAYBACK_NAVIGATION_HISTORY_LIMIT, PlaybackHistoryEntry, PlaybackNavigationHistory,
    PlaybackSpan,
};
pub use playback_loop_policy::PlaybackLoopPolicy;
pub use playback_type_filter::{PLAYBACK_TYPE_FILTERS, PlaybackType, PlaybackTypeFilter};
pub use played_range_history::PlayedRangeHistory;
pub use playmark_timing::{
    MAX_PLAYMARK_TIMING_CHARACTERS, parse_playmark_frame_range, playmark_selection_label,
};
pub use random_audition::{RandomAuditionUnits, random_audition_span_for_units};
pub use random_navigation::RandomNavigationState;
pub use rating::{MAX_RATING, MIN_RATING, RatingState, RatingTransition, next_rating_state};
pub use rating_filter::{RATING_FILTER_LEVELS, RatingFilter};
pub use sample_name::{SampleNameViewMode, sample_display_name};
pub use selection::BrowserSelection;
pub use tag_category::{
    DEFAULT_METADATA_TAGS, MAX_TAG_CATEGORY_INPUT_BYTES, TAG_CATEGORIES, TagCategory,
    TagCategoryError, infer_tag_category,
};
pub use tag_filter::{
    MAX_TAG_FILTER_INPUT_BYTES, MAX_TAG_FILTER_TOKEN_BYTES, MAX_TAG_FILTER_TOKENS,
    TagFilterApplyError, TagFilterError, TagFilterMask, TagFilterMaskError, TagFilterQuery,
    TagFilterState, parse_tag_filter,
};
pub use tag_input::{
    MAX_TAG_INPUT_BYTES, MAX_TAG_LABEL_BYTES, MAX_TAGS_PER_COMMIT, TagInputError, parse_tag_input,
};
pub use waveform_edit_effects::{WaveformEditFade, WaveformEditSettings};
pub use waveform_edit_fade_drag::{WaveformEditFadeDrag, WaveformEditFadeHandle};
pub use waveform_edit_region::WaveformEditRegion;
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
    /// Optional catalog-owned playback classification; direct filesystem scans leave this unset.
    pub playback_type: Option<PlaybackType>,
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
    /// Order by the recognized playback classification.
    PlaybackType,
    /// Order by the first assigned collection slot.
    Collection,
    /// Order by elapsed time since the catalog last recorded playback.
    LastPlayed,
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
    playback_type_filter: PlaybackTypeFilter,
    tag_filter: TagFilterState,
    curation_queue: CurationQueueMode,
    curation_tag_facts: Option<Vec<CurationTagFacts>>,
    reveal_id: Option<AudioFileId>,
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
            playback_type_filter: PlaybackTypeFilter::disabled(),
            tag_filter: TagFilterState::disabled(),
            curation_queue: CurationQueueMode::default(),
            curation_tag_facts: None,
            reveal_id: None,
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

    /// Return the active source playback-type filter, or a disabled filter when no type is
    /// selected.
    pub fn playback_type_filter(&self) -> PlaybackTypeFilter {
        self.playback_type_filter
    }

    /// Return the active external tag-filter result state.
    pub fn tag_filter(&self) -> &TagFilterState {
        &self.tag_filter
    }

    /// Return the source-scoped Curation queue mode.
    pub fn curation_queue(&self) -> CurationQueueMode {
        self.curation_queue
    }

    /// Return whether an enabled Curation queue is waiting for exact per-file tag facts.
    pub fn curation_queue_pending(&self) -> bool {
        self.curation_queue.enabled && self.curation_tag_facts.is_none()
    }

    /// Set the Curation queue mode without changing the ordinary sort choice.
    pub fn set_curation_queue(&mut self, entries: &[AudioFile], mode: CurationQueueMode) -> bool {
        if self.curation_queue == mode {
            return false;
        }
        self.reveal_id = None;
        self.curation_queue = mode;
        self.rebuild(entries);
        true
    }

    /// Apply exact facts for every current entry, in scan order.
    ///
    /// The caller must fence these facts to the current source, root, revision, and lifecycle.
    pub fn apply_curation_tag_facts(
        &mut self,
        entries: &[AudioFile],
        facts: Vec<CurationTagFacts>,
    ) -> bool {
        if facts.len() != entries.len() {
            return false;
        }
        self.curation_tag_facts = Some(facts);
        if self.curation_queue.enabled {
            self.rebuild(entries);
        }
        true
    }

    /// Invalidate exact tag facts after source or tag mutation.
    pub fn invalidate_curation_tag_facts(&mut self, entries: &[AudioFile]) -> bool {
        if self.curation_tag_facts.take().is_none() {
            return false;
        }
        if self.curation_queue.enabled {
            self.rebuild(entries);
        }
        true
    }

    /// Return the single file identity temporarily included in the projection despite another
    /// active filter.
    pub fn reveal_id(&self) -> Option<&AudioFileId> {
        self.reveal_id.as_ref()
    }

    /// Return visible rows in display order.
    pub fn rows(&self) -> &[BrowserRow] {
        &self.rows
    }

    /// Replace the search query and rebuild visible rows.
    pub fn set_query(&mut self, entries: &[AudioFile], query: impl Into<String>) {
        self.reveal_id = None;
        self.query = query.into();
        self.rebuild(entries);
    }

    /// Replace the scan while preserving the current query and sort choice.
    pub fn set_entries(&mut self, entries: &[AudioFile]) {
        self.invalidate_tag_filter_result();
        self.curation_tag_facts = None;
        self.clear_reveal_if_missing(entries);
        self.rebuild(entries);
    }

    /// Begin an enabled tag filter while its external positional result is being computed.
    ///
    /// The temporary reveal is cleared because this is an explicit filter edit. Until a
    /// validated mask is applied, an enabled filter contributes no matching rows.
    pub fn begin_tag_filter(&mut self, entries: &[AudioFile]) -> bool {
        let changed =
            !matches!(self.tag_filter, TagFilterState::Pending) || self.reveal_id.is_some();
        self.reveal_id = None;
        self.tag_filter = TagFilterState::Pending;
        if changed {
            self.rebuild(entries);
        }
        changed
    }

    /// Disable tag filtering and rebuild the projection without its external match mask.
    pub fn disable_tag_filter(&mut self, entries: &[AudioFile]) -> bool {
        let changed =
            !matches!(self.tag_filter, TagFilterState::Disabled) || self.reveal_id.is_some();
        self.reveal_id = None;
        self.tag_filter = TagFilterState::Disabled;
        if changed {
            self.rebuild(entries);
        }
        changed
    }

    /// Apply a validated positional tag-filter result while the filter is enabled.
    ///
    /// The mask must address exactly `entries.len()` entries. Applying a result preserves any
    /// existing reveal and rebuilds the projection; a disabled filter or mismatched mask leaves
    /// the current state untouched.
    pub fn apply_tag_filter(
        &mut self,
        entries: &[AudioFile],
        mask: TagFilterMask,
    ) -> Result<(), TagFilterApplyError> {
        if !self.tag_filter.is_enabled() {
            return Err(TagFilterApplyError::Disabled);
        }
        if mask.entry_count() != entries.len() {
            return Err(TagFilterApplyError::EntryCountMismatch {
                expected: entries.len(),
                actual: mask.entry_count(),
            });
        }
        self.tag_filter = TagFilterState::Ready(mask);
        self.rebuild(entries);
        Ok(())
    }

    /// Invalidate an enabled tag-filter result while preserving its reveal and enabled state.
    pub fn invalidate_tag_filter(&mut self, entries: &[AudioFile]) -> bool {
        let changed = self.invalidate_tag_filter_result();
        if changed {
            self.rebuild(entries);
        }
        changed
    }

    /// Set the selected folder and whether descendants are included, preserving search and sort.
    /// A changed folder or scope clears the temporary reveal; an unchanged scope preserves it for
    /// metadata refreshes.
    pub fn set_folder_scope(
        &mut self,
        entries: &[AudioFile],
        folder: Option<PathBuf>,
        scope: FolderScope,
    ) {
        if self.folder.as_deref() != folder.as_deref() || self.folder_scope != scope {
            self.reveal_id = None;
        }
        self.clear_reveal_if_missing(entries);
        self.folder = folder;
        self.folder_scope = scope;
        self.rebuild(entries);
    }

    /// Temporarily include one exact entry while retaining the current folder and filters.
    ///
    /// The identity must belong to `entries`; an invalid identity leaves the projection and
    /// existing reveal unchanged. The entry is still subject to the selected folder scope, so a
    /// reveal cannot expose a file from another folder.
    pub fn set_reveal(&mut self, entries: &[AudioFile], reveal_id: Option<AudioFileId>) -> bool {
        if let Some(reveal_id) = reveal_id.as_ref()
            && !entries
                .iter()
                .any(|entry| entry.path.as_path() == reveal_id.path())
        {
            return false;
        }
        if self.reveal_id == reveal_id {
            return false;
        }
        self.reveal_id = reveal_id;
        self.rebuild(entries);
        true
    }

    /// Clear the temporary single-entry reveal.
    pub fn clear_reveal(&mut self, entries: &[AudioFile]) -> bool {
        self.set_reveal(entries, None)
    }

    fn clear_reveal_if_missing(&mut self, entries: &[AudioFile]) {
        if self.reveal_id.as_ref().is_some_and(|reveal_id| {
            !entries
                .iter()
                .any(|entry| entry.path.as_path() == reveal_id.path())
        }) {
            self.reveal_id = None;
        }
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
        self.reveal_id = None;
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
        self.reveal_id = None;
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

    /// Replace the playback-type filter while preserving query, folder, collection, rating, and
    /// sort state.
    pub fn set_playback_type_filter(
        &mut self,
        entries: &[AudioFile],
        filter: PlaybackTypeFilter,
    ) -> bool {
        if self.playback_type_filter == filter {
            return false;
        }
        self.reveal_id = None;
        self.playback_type_filter = filter;
        self.rebuild(entries);
        true
    }

    /// Toggle one playback-type filter chip and return whether the projection changed.
    pub fn toggle_playback_type_filter(
        &mut self,
        entries: &[AudioFile],
        playback_type: PlaybackType,
    ) -> bool {
        let next = self.playback_type_filter.toggled_type(playback_type);
        self.set_playback_type_filter(entries, next)
    }

    /// Count each collection across the current name, folder, rating, playback-type, and tag
    /// scope.
    ///
    /// The active collection filter is intentionally ignored so sidebar counts remain useful
    /// while a different collection is selected. Callers should cache this result and refresh it
    /// when entries, query, folder, rating, or playback-type scope changes rather than during row
    /// rendering.
    pub fn collection_counts(&self, entries: &[AudioFile]) -> [usize; COLLECTION_COUNT] {
        let query = self.query.trim().to_ascii_lowercase();
        let mut counts = [0; COLLECTION_COUNT];
        for (entry_index, entry) in entries.iter().enumerate() {
            if !self.entry_matches_tag_filter(entry_index)
                || !self.entry_matches_scope(entry, &query)
                || !self.entry_matches_rating(entry)
                || !self.entry_matches_playback_type(entry)
                || !self.entry_matches_curation_queue(
                    entry_index,
                    entry,
                    current_unix_epoch_seconds(),
                )
            {
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
        self.rebuild_with_now(entries, current_unix_epoch_seconds());
    }

    fn invalidate_tag_filter_result(&mut self) -> bool {
        match self.tag_filter {
            TagFilterState::Disabled | TagFilterState::Pending => false,
            TagFilterState::Ready(_) => {
                self.tag_filter = TagFilterState::Pending;
                true
            }
        }
    }

    fn rebuild_with_now(&mut self, entries: &[AudioFile], now_epoch_seconds: i64) {
        let query = self.query.trim().to_ascii_lowercase();
        let reveal_id = self.reveal_id.as_ref();
        let mut projected = entries
            .iter()
            .enumerate()
            .filter(|(entry_index, entry)| {
                let in_folder = self.folder.as_deref().is_none_or(|folder| {
                    folder_contains_relative_path(folder, &entry.relative_path, self.folder_scope)
                });
                in_folder
                    && (reveal_id.is_some_and(|reveal_id| entry.path.as_path() == reveal_id.path())
                        || (self.entry_matches_tag_filter(*entry_index)
                            && audio_file_matches_name_query(entry, &query)
                            && self.collection_filter.is_none_or(|slot| {
                                entry
                                    .curation
                                    .is_some_and(|curation| curation.has_collection(slot))
                            })
                            && self.entry_matches_rating(entry)
                            && self.entry_matches_playback_type(entry)
                            && self.entry_matches_curation_queue(
                                *entry_index,
                                entry,
                                now_epoch_seconds,
                            )))
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
                    last_played_sort_key(
                        entry.curation.and_then(AudioFileCuration::last_played_at),
                        now_epoch_seconds,
                    ),
                    playback_type_sort_key(entry.playback_type),
                    self.curation_queue_key(entry_index, entry, now_epoch_seconds),
                )
            })
            .collect::<Vec<_>>();
        projected.sort_by(|left, right| {
            let ordering = if self.curation_queue.enabled {
                left.9.cmp(&right.9)
            } else {
                match self.sort.column {
                    SortColumn::Name => left.2.cmp(&right.2),
                    SortColumn::Size => left.4.cmp(&right.4),
                    SortColumn::Extension => left.3.cmp(&right.3),
                    SortColumn::Rating => left.5.cmp(&right.5),
                    SortColumn::Collection => left.6.cmp(&right.6),
                    SortColumn::LastPlayed => left.7.cmp(&right.7),
                    SortColumn::PlaybackType => left.8.cmp(&right.8),
                }
            }
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.1.path().cmp(right.1.path()));
            match if self.curation_queue.enabled {
                SortDirection::Ascending
            } else {
                self.sort.direction
            } {
                SortDirection::Ascending => ordering,
                SortDirection::Descending => ordering.reverse(),
            }
        });
        self.rows = projected
            .into_iter()
            .map(|(entry_index, id, _, _, _, _, _, _, _, _)| BrowserRow { entry_index, id })
            .collect();
    }

    fn curation_queue_key(
        &self,
        entry_index: usize,
        entry: &AudioFile,
        now_epoch_seconds: i64,
    ) -> Option<CurationQueueKey> {
        let tags = self.curation_tag_facts.as_ref()?.get(entry_index).copied();
        match curation_queue_decision(entry, tags, self.curation_queue, now_epoch_seconds) {
            CurationQueueDecision::Included(key) => Some(key),
            _ => None,
        }
    }

    fn entry_matches_curation_queue(
        &self,
        entry_index: usize,
        entry: &AudioFile,
        now_epoch_seconds: i64,
    ) -> bool {
        !self.curation_queue.enabled
            || self
                .curation_queue_key(entry_index, entry, now_epoch_seconds)
                .is_some()
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

    fn entry_matches_playback_type(&self, entry: &AudioFile) -> bool {
        self.playback_type_filter.matches(entry.playback_type)
    }

    fn entry_matches_tag_filter(&self, entry_index: usize) -> bool {
        match &self.tag_filter {
            TagFilterState::Disabled => true,
            TagFilterState::Pending => false,
            TagFilterState::Ready(mask) => mask.contains(entry_index),
        }
    }
}

fn current_unix_epoch_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
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

fn playback_type_sort_key(playback_type: Option<PlaybackType>) -> u8 {
    match playback_type {
        Some(PlaybackType::Loop) => 0,
        Some(PlaybackType::OneShot) => 1,
        None => 2,
    }
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
            playback_type: None,
            curation: None,
        }
    }

    fn typed_entry(
        path: &str,
        playback_type: Option<PlaybackType>,
        curation: Option<AudioFileCuration>,
    ) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(path),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type,
            curation,
        }
    }

    #[test]
    fn curation_queue_intersects_existing_filters_orders_rows_and_fails_closed() {
        let entries = vec![
            typed_entry(
                "/source/kit/complete.wav",
                Some(PlaybackType::OneShot),
                AudioFileCuration::from_parts(2, false, 0b1, None, None),
            ),
            typed_entry(
                "/source/kit/empty.wav",
                Some(PlaybackType::OneShot),
                AudioFileCuration::from_parts(0, false, 0b1, None, None),
            ),
            typed_entry(
                "/source/kit/locked.wav",
                Some(PlaybackType::Loop),
                AudioFileCuration::from_parts(3, true, 0b1, None, None),
            ),
        ];
        let mut model = BrowserViewModel::new(&entries);
        let mode = CurationQueueMode {
            enabled: true,
            ..Default::default()
        };
        assert!(model.set_curation_queue(&entries, mode));
        assert!(model.curation_queue_pending());
        assert!(model.rows().is_empty());
        assert!(!model.apply_curation_tag_facts(&entries, vec![]));
        assert!(model.rows().is_empty());
        assert!(model.apply_curation_tag_facts(
            &entries,
            vec![
                CurationTagFacts::from_flags(true, true, true),
                CurationTagFacts::from_flags(false, false, false),
                CurationTagFacts::from_flags(true, true, true),
            ]
        ));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert_eq!(model.collection_counts(&entries), [2, 0, 0, 0, 0, 0]);

        model.set_query(&entries, "complete");
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 0);
        assert!(model.set_reveal(&entries, Some(entries[1].stable_id())));
        assert_eq!(model.rows().len(), 2);
        assert!(model.set_curation_queue(
            &entries,
            CurationQueueMode {
                scope: CurationQueueScope::Tags,
                ..mode
            }
        ));
        assert!(model.rows().is_empty());
        model.set_query(&entries, "");
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 1);

        assert!(model.invalidate_curation_tag_facts(&entries));
        assert!(model.curation_queue_pending());
        assert!(model.rows().is_empty());
        assert!(model.set_curation_queue(&entries, CurationQueueMode::default()));
        assert_eq!(model.rows().len(), 3);
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
    fn playback_type_sort_uses_loop_then_one_shot_then_unknown_order() {
        let entries = vec![
            typed_entry("/tmp/unknown.wav", None, None),
            typed_entry("/tmp/one-shot.wav", Some(PlaybackType::OneShot), None),
            typed_entry("/tmp/loop.wav", Some(PlaybackType::Loop), None),
        ];
        let mut model = BrowserViewModel::new(&entries);

        model.toggle_sort(&entries, SortColumn::PlaybackType);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![2, 1, 0]
        );

        model.toggle_sort(&entries, SortColumn::PlaybackType);
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
    fn collection_filter_intersects_name_and_folder_scope_and_restores_focus() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/keep.wav"),
                relative_path: PathBuf::from("kit/keep.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(2, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/other.wav"),
                relative_path: PathBuf::from("kit/other.wav"),
                byte_len: 2,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/elsewhere/keep.wav"),
                relative_path: PathBuf::from("elsewhere/keep.wav"),
                byte_len: 3,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
                playback_type: None,
                curation: AudioFileCuration::from_parts(-3, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/k3.wav"),
                relative_path: PathBuf::from("k3.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(3, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/k4.wav"),
                relative_path: PathBuf::from("k4.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
                playback_type: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/kick-b.wav"),
                relative_path: PathBuf::from("kit/kick-b.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/elsewhere/kick-c.wav"),
                relative_path: PathBuf::from("elsewhere/kick-c.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/pad.wav"),
                relative_path: PathBuf::from("kit/pad.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
    fn playback_type_filter_toggles_independently_and_excludes_unknown_when_active() {
        let entries = vec![
            typed_entry("/tmp/one-shot.wav", Some(PlaybackType::OneShot), None),
            typed_entry("/tmp/loop.wav", Some(PlaybackType::Loop), None),
            typed_entry("/tmp/unknown.wav", None, None),
        ];
        let mut model = BrowserViewModel::new(&entries);
        assert_eq!(model.rows().len(), 3);

        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));
        assert_eq!(model.playback_type_filter().mask(), 0b01);
        assert_eq!(model.rows()[0].entry_index, 0);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));
        assert_eq!(model.playback_type_filter().mask(), 0b11);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );

        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));
        assert_eq!(model.rows()[0].entry_index, 1);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));
        assert!(model.playback_type_filter().is_disabled());
        assert_eq!(model.rows().len(), 3);
    }

    #[test]
    fn playback_type_filter_intersects_other_scopes_and_collection_counts() {
        let curation = |rating, collections| {
            AudioFileCuration::from_parts(rating, false, collections, None, None)
        };
        let entries = vec![
            typed_entry(
                "kit/kick-one.wav",
                Some(PlaybackType::OneShot),
                curation(-1, 0b0000_0011),
            ),
            typed_entry(
                "kit/kick-loop.wav",
                Some(PlaybackType::Loop),
                curation(-1, 0b0000_0110),
            ),
            typed_entry("kit/kick-unknown.wav", None, curation(-1, 0b0000_1001)),
            typed_entry(
                "other/kick-loop.wav",
                Some(PlaybackType::Loop),
                curation(-1, 0b0000_0001),
            ),
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        model.set_query(&entries, "kick");
        assert!(model.set_collection_filter(&entries, Some(1)));
        assert!(model.toggle_rating_filter(&entries, -1));
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));

        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(model.collection_counts(&entries), [0, 1, 1, 0, 0, 0]);
    }

    #[test]
    fn tag_filter_intersects_name_folder_rating_playback_and_collection_scopes() {
        let curation =
            |collections| AudioFileCuration::from_parts(-1, false, collections, None, None);
        let entries = vec![
            typed_entry(
                "kit/kick-loop-a.wav",
                Some(PlaybackType::Loop),
                curation(0b0000_0001),
            ),
            typed_entry(
                "kit/kick-loop-b.wav",
                Some(PlaybackType::Loop),
                curation(0b0000_0001),
            ),
            typed_entry(
                "kit/kick-one.wav",
                Some(PlaybackType::OneShot),
                curation(0b0000_0001),
            ),
            typed_entry(
                "other/kick-loop.wav",
                Some(PlaybackType::Loop),
                curation(0b0000_0001),
            ),
            typed_entry(
                "kit/pad-loop.wav",
                Some(PlaybackType::Loop),
                curation(0b0000_0001),
            ),
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
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));
        assert!(model.begin_tag_filter(&entries));
        assert!(model.tag_filter().is_pending());
        assert!(model.rows().is_empty());

        let mask = TagFilterMask::try_from_vec(entries.len(), vec![1u64 << 1]).unwrap();
        model.apply_tag_filter(&entries, mask).unwrap();
        assert!(matches!(model.tag_filter(), TagFilterState::Ready(_)));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn tag_filter_pending_and_disabled_states_have_distinct_projection_behavior() {
        let entries = vec![model_entry("/tmp/a.wav", 1), model_entry("/tmp/b.wav", 2)];
        let mut model = BrowserViewModel::new(&entries);
        assert!(matches!(model.tag_filter(), TagFilterState::Disabled));
        assert_eq!(model.rows().len(), entries.len());

        assert!(model.begin_tag_filter(&entries));
        assert!(model.tag_filter().is_enabled());
        assert!(model.tag_filter().is_pending());
        assert!(model.rows().is_empty());

        assert!(model.disable_tag_filter(&entries));
        assert!(matches!(model.tag_filter(), TagFilterState::Disabled));
        assert_eq!(model.rows().len(), entries.len());
    }

    #[test]
    fn tag_filter_reveal_bypasses_tag_mask_but_keeps_folder_boundary() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/source/kit/inside.wav"),
                relative_path: PathBuf::from("kit/inside.wav"),
                ..model_entry("/source/kit/inside.wav", 1)
            },
            AudioFile {
                path: PathBuf::from("/source/other/outside.wav"),
                relative_path: PathBuf::from("other/outside.wav"),
                ..model_entry("/source/other/outside.wav", 2)
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        assert!(model.begin_tag_filter(&entries));

        let outside_id = entries[1].stable_id();
        assert!(model.set_reveal(&entries, Some(outside_id)));
        assert!(model.rows().is_empty());

        let inside_id = entries[0].stable_id();
        assert!(model.set_reveal(&entries, Some(inside_id.clone())));
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 0);
        assert_eq!(model.reveal_id(), Some(&inside_id));
    }

    #[test]
    fn same_length_entry_replacement_invalidates_positional_tag_mask() {
        let entries = vec![model_entry("/tmp/a.wav", 1), model_entry("/tmp/b.wav", 2)];
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.begin_tag_filter(&entries));
        model
            .apply_tag_filter(
                &entries,
                TagFilterMask::try_from_vec(entries.len(), vec![1]).unwrap(),
            )
            .unwrap();
        assert_eq!(model.rows()[0].entry_index, 0);

        let replacement = vec![entries[1].clone(), entries[0].clone()];
        model.set_entries(&replacement);
        assert!(model.tag_filter().is_pending());
        assert!(model.rows().is_empty());
    }

    #[test]
    fn sorting_reorders_ready_tag_rows_without_reinterpreting_mask_positions() {
        let entries = vec![
            model_entry("/tmp/zulu.wav", 1),
            model_entry("/tmp/alpha.wav", 2),
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.begin_tag_filter(&entries);
        model
            .apply_tag_filter(
                &entries,
                TagFilterMask::try_from_vec(entries.len(), vec![1]).unwrap(),
            )
            .unwrap();
        let selected_id = entries[0].stable_id();
        model.toggle_sort(&entries, SortColumn::Name);
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].entry_index, 0);
        assert_eq!(model.rows()[0].id, selected_id);
    }

    #[test]
    fn tag_filter_apply_rejects_disabled_and_mismatched_results() {
        let entries = vec![model_entry("/tmp/a.wav", 1)];
        let mut model = BrowserViewModel::new(&entries);
        let mask = TagFilterMask::try_from_vec(1, vec![1]).unwrap();
        assert_eq!(
            model.apply_tag_filter(&entries, mask.clone()),
            Err(TagFilterApplyError::Disabled)
        );
        model.begin_tag_filter(&entries);
        let mismatched = TagFilterMask::try_from_vec(2, vec![1]).unwrap();
        assert_eq!(
            model.apply_tag_filter(&entries, mismatched),
            Err(TagFilterApplyError::EntryCountMismatch {
                expected: 1,
                actual: 2,
            })
        );
        assert!(model.tag_filter().is_pending());
        assert!(model.apply_tag_filter(&entries, mask).is_ok());
    }

    #[test]
    fn reveal_includes_one_filtered_row_in_scope_without_inflating_counts() {
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/source/kit/visible.wav"),
                relative_path: PathBuf::from("kit/visible.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: Some(PlaybackType::OneShot),
                curation: AudioFileCuration::from_parts(3, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/source/kit/hidden.wav"),
                relative_path: PathBuf::from("kit/hidden.wav"),
                byte_len: 2,
                duration_seconds: None,
                sample_rate: None,
                playback_type: Some(PlaybackType::Loop),
                curation: AudioFileCuration::from_parts(0, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/source/other/hidden.wav"),
                relative_path: PathBuf::from("other/hidden.wav"),
                byte_len: 3,
                duration_seconds: None,
                sample_rate: None,
                playback_type: Some(PlaybackType::Loop),
                curation: AudioFileCuration::from_parts(0, false, 0b0000_0010, None, None),
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(
            &entries,
            Some(PathBuf::from("kit")),
            FolderScope::DirectOnly,
        );
        model.set_query(&entries, "visible");
        assert!(model.set_collection_filter(&entries, Some(0)));
        assert!(model.toggle_rating_filter(&entries, 3));
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(model.collection_counts(&entries), [1, 0, 0, 0, 0, 0]);

        let target_id = entries[1].stable_id();
        assert!(model.set_reveal(&entries, Some(target_id.clone())));
        assert_eq!(model.reveal_id(), Some(&target_id));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert_eq!(model.collection_counts(&entries), [1, 0, 0, 0, 0, 0]);

        model.toggle_sort(&entries, SortColumn::Rating);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );

        let before_invalid_reveal = model.clone();
        let missing_id = model_entry("/source/missing.wav", 4).stable_id();
        assert!(!model.set_reveal(&entries, Some(missing_id)));
        assert_eq!(model, before_invalid_reveal);

        assert!(model.set_collection_filter(&entries, Some(1)));
        assert!(model.reveal_id().is_none());
        assert!(model.set_reveal(&entries, Some(target_id.clone())));
        model.set_query(&entries, "visible");
        assert!(model.reveal_id().is_none());
        assert!(model.set_collection_filter(&entries, Some(0)));

        let outside_id = entries[2].stable_id();
        assert!(model.set_reveal(&entries, Some(outside_id)));
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert!(model.clear_reveal(&entries));
        assert!(model.reveal_id().is_none());
        model.set_query(&entries, "hidden");
        assert_eq!(model.rows().len(), 0);
    }

    #[test]
    fn reveal_survives_metadata_replacement_and_clears_when_entry_disappears() {
        let entries = vec![
            model_entry("/source/a.wav", 1),
            model_entry("/source/b.wav", 2),
        ];
        let target_id = entries[1].stable_id();
        let mut model = BrowserViewModel::new(&entries);
        model.set_folder_scope(&entries, Some(PathBuf::from(".")), FolderScope::DirectOnly);
        assert!(model.set_reveal(&entries, Some(target_id.clone())));

        model.set_folder_scope(&entries, Some(PathBuf::from(".")), FolderScope::DirectOnly);
        assert_eq!(model.reveal_id(), Some(&target_id));

        let mut replacement = entries.clone();
        replacement[1].byte_len = 99;
        model.set_entries(&replacement);
        assert_eq!(model.reveal_id(), Some(&target_id));
        assert!(model.visible_position_for(&target_id).is_some());

        model.set_folder_scope(
            &replacement[..1],
            Some(PathBuf::from(".")),
            FolderScope::DirectOnly,
        );
        assert!(model.reveal_id().is_none());
        assert_eq!(model.rows().len(), 1);

        model.set_entries(&replacement[..1]);
        assert!(model.reveal_id().is_none());
        assert_eq!(model.rows().len(), 1);
    }

    #[test]
    fn playback_type_filter_restores_focus_after_the_focused_row_disappears() {
        let entries = vec![
            typed_entry("/tmp/one.wav", Some(PlaybackType::OneShot), None),
            typed_entry("/tmp/loop.wav", Some(PlaybackType::Loop), None),
        ];
        let mut model = BrowserViewModel::new(&entries);
        let focused_id = entries[1].stable_id();
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::OneShot));
        assert_eq!(model.restore_focus(Some(&focused_id), Some(1)), Some(0));
    }

    #[test]
    fn playback_type_filter_survives_source_entry_replacement() {
        let entries = vec![
            typed_entry("/tmp/one.wav", Some(PlaybackType::OneShot), None),
            typed_entry("/tmp/loop.wav", Some(PlaybackType::Loop), None),
        ];
        let replacement = vec![typed_entry(
            "/tmp/reloaded-loop.wav",
            Some(PlaybackType::Loop),
            None,
        )];
        let mut model = BrowserViewModel::new(&entries);
        assert!(model.toggle_playback_type_filter(&entries, PlaybackType::Loop));
        model.set_entries(&replacement);

        assert_eq!(model.playback_type_filter().mask(), 0b10);
        assert_eq!(model.rows().len(), 1);
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
                playback_type: None,
                curation: AudioFileCuration::from_parts(0, false, 0, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/bravo.wav"),
                relative_path: PathBuf::from("bravo.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
                playback_type: None,
                curation: AudioFileCuration::from_parts(1, false, 0b0000_0010, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/alpha.wav"),
                relative_path: PathBuf::from("alpha.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: AudioFileCuration::from_parts(-1, false, 0b0000_0001, None, None),
            },
            AudioFile {
                path: PathBuf::from("/tmp/bravo.wav"),
                relative_path: PathBuf::from("bravo.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
    fn last_played_sort_is_recent_first_with_deterministic_ties_and_nulls() {
        const NOW: i64 = 1_700_000_000;
        let curation =
            |last_played_at| AudioFileCuration::from_parts(0, false, 0, last_played_at, None);
        let entries = vec![
            AudioFile {
                path: PathBuf::from("/tmp/z/alpha.wav"),
                relative_path: PathBuf::from("z/alpha.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: curation(Some(NOW - 2 * SECONDS_PER_DAY as i64)),
            },
            AudioFile {
                path: PathBuf::from("/tmp/a/alpha.wav"),
                relative_path: PathBuf::from("a/alpha.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: curation(Some(NOW - 2 * SECONDS_PER_DAY as i64)),
            },
            AudioFile {
                path: PathBuf::from("/tmp/recent.wav"),
                relative_path: PathBuf::from("recent.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: curation(Some(NOW - 1)),
            },
            AudioFile {
                path: PathBuf::from("/tmp/never.wav"),
                relative_path: PathBuf::from("never.wav"),
                byte_len: 1,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: curation(None),
            },
        ];
        let mut model = BrowserViewModel::new(&entries);
        model.sort = SortSpec {
            column: SortColumn::LastPlayed,
            direction: SortDirection::Ascending,
        };
        model.rebuild_with_now(&entries, NOW);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![2, 1, 0, 3]
        );

        model.sort.direction = SortDirection::Descending;
        model.rebuild_with_now(&entries, NOW);
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.entry_index)
                .collect::<Vec<_>>(),
            vec![3, 0, 1, 2]
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
                playback_type: None,
                curation: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/bravo.wav"),
                relative_path: PathBuf::from("kit/bravo.wav"),
                byte_len: 10,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            },
            AudioFile {
                path: PathBuf::from("/tmp/source/kit/charlie.wav"),
                relative_path: PathBuf::from("kit/charlie.wav"),
                byte_len: 20,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
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
