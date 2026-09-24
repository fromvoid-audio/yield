#![deny(missing_docs)]
#![deny(warnings)]

//! Shared bounded merge and cursor state for tag vocabulary views.

use std::cmp::Ordering;

use yield_core::DEFAULT_METADATA_TAGS;
use yield_library::sample_sources::{
    ProfileTagVocabularyEntry, ProfileTagVocabularyPageCursor, SourceTagPage, SourceTagPageCursor,
    SourceTagPageEntry,
};

/// Maximum number of composite cursor checkpoints retained by one pager.
pub(super) const MAX_CURSOR_HISTORY: usize = 32;
/// Maximum number of exact labels retained for bounded cross-page de-duplication.
pub(super) const MAX_SEEN_EXACT: usize = 128;

/// One bounded source vocabulary window.
pub(super) struct SourcePageWindow {
    /// Source metadata revision for this window.
    pub(super) revision: u64,
    /// Bounded source entries in canonical order.
    pub(super) entries: Vec<SourceTagPageEntry>,
    /// Opaque cursor for the next source window.
    pub(super) next_cursor: Option<SourceTagPageCursor>,
    /// Cursor used to request this window.
    pub(super) page_start: Option<SourceTagPageCursor>,
    /// Number of entries already consumed from this window.
    pub(super) offset: usize,
}

/// One bounded profile vocabulary window.
pub(super) struct ProfilePageWindow {
    /// Profile vocabulary revision for this window.
    pub(super) revision: u64,
    /// Bounded profile entries in canonical order.
    pub(super) entries: Vec<ProfileTagVocabularyEntry>,
    /// Opaque cursor for the next profile window.
    pub(super) next_cursor: Option<ProfileTagVocabularyPageCursor>,
    /// Cursor used to request this window.
    pub(super) page_start: Option<ProfileTagVocabularyPageCursor>,
    /// Number of entries already consumed from this window.
    pub(super) offset: usize,
}

/// A bounded position in all three merged vocabulary lanes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StreamCheckpoint {
    /// Whether a source window must be loaded when restoring.
    pub(super) source_loaded: bool,
    /// Source cursor at the beginning of the retained window.
    pub(super) source_after_cursor: Option<SourceTagPageCursor>,
    /// Source offset in the retained window.
    pub(super) source_offset: usize,
    /// Whether a profile window must be loaded when restoring.
    pub(super) profile_loaded: bool,
    /// Profile revision expected when restoring.
    pub(super) profile_revision: Option<u64>,
    /// Profile cursor at the beginning of the retained window.
    pub(super) profile_after_cursor: Option<ProfileTagVocabularyPageCursor>,
    /// Profile offset in the retained window.
    pub(super) profile_offset: usize,
    /// Offset in the built-in default lane.
    pub(super) default_offset: usize,
    /// Exact labels consumed before this checkpoint.
    pub(super) seen_exact: Vec<String>,
}

impl StreamCheckpoint {
    /// Construct the first position in a merged vocabulary stream.
    pub(super) fn initial() -> Self {
        Self {
            source_loaded: true,
            source_after_cursor: None,
            source_offset: 0,
            profile_loaded: true,
            profile_revision: None,
            profile_after_cursor: None,
            profile_offset: 0,
            default_offset: 0,
            seen_exact: Vec::new(),
        }
    }
}

/// One candidate emitted by a bounded merged vocabulary stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MergedCandidate {
    /// A built-in metadata tag.
    Default(&'static str),
    /// A profile vocabulary label.
    Profile {
        /// Exact profile label.
        label: String,
    },
    /// A source-local tag identity and bounded display data.
    Source {
        /// Stable source-local tag id.
        tag_id: i64,
        /// Bounded display label.
        label: String,
        /// Exact normalized label when the source row is not a legacy preview.
        exact_label: Option<String>,
        /// Source metadata revision that yielded the identity.
        revision: u64,
    },
}

/// Stable identity captured by a row callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CandidateToken {
    /// Built-in label identity.
    Default(&'static str),
    /// Profile label identity.
    Profile(String),
    /// Source-local numeric identity.
    Source(i64),
}

/// One lane in the merged stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Default,
    Profile,
    Source,
}

/// Direction for bounded keyboard selection movement.
#[derive(Clone, Copy)]
pub(super) enum SelectionDirection {
    /// Move toward the beginning of the visible page.
    Previous,
    /// Move toward the end of the visible page.
    Next,
}

/// Result of filling one visible merged page.
pub(super) struct MergePageResult {
    /// Cursor needed to continue the source lane.
    pub(super) source_after: Option<SourceTagPageCursor>,
    /// Cursor needed to continue the profile lane.
    pub(super) profile_after: Option<ProfileTagVocabularyPageCursor>,
}

/// Shared bounded stream state used by completion and the Tag Editor panel.
pub(super) struct TagPagerState {
    /// Retained source window.
    pub(super) source_page: Option<SourcePageWindow>,
    /// Retained profile window.
    pub(super) profile_page: Option<ProfilePageWindow>,
    /// Offset in the built-in default lane.
    pub(super) default_offset: usize,
    /// Candidates in the current visible page.
    pub(super) merged_page: Vec<MergedCandidate>,
    /// Checkpoint for the current visible page.
    pub(super) current_checkpoint: Option<StreamCheckpoint>,
    /// Bounded previous-page checkpoints.
    pub(super) page_history: Vec<StreamCheckpoint>,
    /// Exact labels already emitted in the current stream.
    pub(super) seen_exact: Vec<String>,
    /// Epoch captured by callbacks for the current page.
    pub(super) page_epoch: u64,
    /// Whether another merged page may be requested.
    pub(super) page_has_next: bool,
    /// Selected candidate index in the current page.
    pub(super) selected: Option<usize>,
}

impl Default for TagPagerState {
    fn default() -> Self {
        Self {
            source_page: None,
            profile_page: None,
            default_offset: 0,
            merged_page: Vec::new(),
            current_checkpoint: None,
            page_history: Vec::new(),
            seen_exact: Vec::new(),
            page_epoch: 0,
            page_has_next: false,
            selected: None,
        }
    }
}

impl TagPagerState {
    /// Clear the visible page and invalidate callbacks for it.
    pub(super) fn invalidate_menu(&mut self) {
        self.merged_page.clear();
        self.selected = None;
        self.page_has_next = false;
        self.page_epoch = self.page_epoch.wrapping_add(1);
    }

    /// Clear all retained stream windows and history.
    pub(super) fn clear_pages(&mut self) {
        self.source_page = None;
        self.profile_page = None;
        self.default_offset = 0;
        self.current_checkpoint = None;
        self.page_history.clear();
        self.seen_exact.clear();
        self.invalidate_menu();
    }

    /// Snapshot the current stream position for a previous-page checkpoint.
    pub(super) fn snapshot(&self) -> StreamCheckpoint {
        StreamCheckpoint {
            source_loaded: self.source_page.is_some(),
            source_after_cursor: self.source_page.as_ref().and_then(|page| page.page_start),
            source_offset: self
                .source_page
                .as_ref()
                .map(|page| page.offset)
                .unwrap_or_default(),
            profile_loaded: self.profile_page.is_some(),
            profile_revision: self.profile_page.as_ref().map(|page| page.revision),
            profile_after_cursor: self
                .profile_page
                .as_ref()
                .and_then(|page| page.page_start.clone()),
            profile_offset: self
                .profile_page
                .as_ref()
                .map(|page| page.offset)
                .unwrap_or_default(),
            default_offset: self.default_offset,
            seen_exact: self.seen_exact.clone(),
        }
    }

    /// Return the number of candidates in the current visible page.
    pub(super) fn candidate_count(&self) -> usize {
        self.merged_page.len()
    }

    /// Clone one candidate for an action callback.
    pub(super) fn candidate(&self, index: usize) -> Option<MergedCandidate> {
        self.merged_page.get(index).cloned()
    }
}

/// Keep the previous-page history within its fixed memory bound.
pub(super) fn push_page_history(history: &mut Vec<StreamCheckpoint>, checkpoint: StreamCheckpoint) {
    if history.len() >= MAX_CURSOR_HISTORY {
        history.remove(0);
    }
    history.push(checkpoint);
}

/// Install one bounded source page at the requested cursor and offset.
pub(super) fn install_source_page(
    state: &mut TagPagerState,
    page: SourceTagPage,
    page_start: Option<SourceTagPageCursor>,
    skip: usize,
) {
    let entry_count = page.entries.len();
    state.source_page = Some(SourcePageWindow {
        revision: page.revision,
        entries: page.entries,
        next_cursor: page.next_cursor,
        page_start,
        offset: skip.min(entry_count),
    });
}

/// Restore the data-only portion of one composite stream checkpoint.
pub(super) fn apply_stream_checkpoint(state: &mut TagPagerState, checkpoint: &StreamCheckpoint) {
    state.source_page = None;
    state.profile_page = None;
    state.default_offset = checkpoint.default_offset;
    state.seen_exact = checkpoint.seen_exact.clone();
    state.merged_page.clear();
    state.selected = None;
    state.page_has_next = false;
    state.page_epoch = state.page_epoch.wrapping_add(1);
    state.current_checkpoint = Some(checkpoint.clone());
}

/// Fill one bounded visible page from the retained source, profile, and default lanes.
pub(super) fn merge_ready_page(
    state: &mut TagPagerState,
    prefix: &str,
    page_size: usize,
) -> MergePageResult {
    let mut source_after = None;
    let mut profile_after = None;
    let mut changed = false;
    while state.merged_page.len() < page_size {
        if let Some(cursor) = source_next_cursor(state) {
            source_after = Some(cursor);
            break;
        }
        if let Some(cursor) = profile_next_cursor(state) {
            profile_after = Some(cursor);
            break;
        }
        let Some(lane) = choose_lane(state, prefix) else {
            break;
        };
        let exact = lane_exact_key(state, lane, prefix);
        let candidate = lane_candidate(state, lane, prefix);
        advance_lane(state, lane);
        let duplicate = exact
            .as_deref()
            .is_some_and(|key| state.seen_exact.iter().any(|seen| seen == key));
        if duplicate {
            continue;
        }
        if let Some(key) = exact {
            if state.seen_exact.len() >= MAX_SEEN_EXACT {
                state.seen_exact.remove(0);
            }
            state.seen_exact.push(key);
        }
        let Some(candidate) = candidate else {
            break;
        };
        state.merged_page.push(candidate);
        changed = true;
    }

    state.page_has_next =
        state.merged_page.len() >= page_size && has_unconsumed_candidate(state, prefix);
    if changed {
        state.selected = None;
        state.page_epoch = state.page_epoch.wrapping_add(1);
    }
    MergePageResult {
        source_after,
        profile_after,
    }
}

fn has_unconsumed_candidate(state: &TagPagerState, prefix: &str) -> bool {
    default_head(prefix, state.default_offset).is_some()
        || profile_head(state, prefix).is_some()
        || source_head(state).is_some()
        || source_next_cursor(state).is_some()
        || profile_next_cursor(state).is_some()
}

fn source_next_cursor(state: &TagPagerState) -> Option<SourceTagPageCursor> {
    state.source_page.as_ref().and_then(|page| {
        (page.offset >= page.entries.len())
            .then_some(page.next_cursor)
            .flatten()
    })
}

fn profile_next_cursor(state: &TagPagerState) -> Option<ProfileTagVocabularyPageCursor> {
    state.profile_page.as_ref().and_then(|page| {
        (page.offset >= page.entries.len())
            .then(|| page.next_cursor.clone())
            .flatten()
    })
}

fn default_head(prefix: &str, offset: usize) -> Option<&'static str> {
    DEFAULT_METADATA_TAGS
        .into_iter()
        .filter(|label| label.starts_with(prefix))
        .nth(offset)
}

fn profile_head<'a>(
    state: &'a TagPagerState,
    prefix: &str,
) -> Option<&'a ProfileTagVocabularyEntry> {
    state.profile_page.as_ref().and_then(|page| {
        page.entries
            .get(page.offset)
            .filter(|entry| entry.label.starts_with(prefix))
    })
}

fn source_head(state: &TagPagerState) -> Option<&SourceTagPageEntry> {
    state
        .source_page
        .as_ref()
        .and_then(|page| page.entries.get(page.offset))
}

fn lane_key<'a>(state: &'a TagPagerState, lane: Lane, prefix: &str) -> Option<&'a str> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset),
        Lane::Profile => profile_head(state, prefix).map(|entry| entry.label.as_str()),
        Lane::Source => source_head(state).map(|entry| entry.normalized_text.as_str()),
    }
}

fn lane_source_id(state: &TagPagerState, lane: Lane) -> i64 {
    match lane {
        Lane::Source => source_head(state).map(|entry| entry.id).unwrap_or_default(),
        Lane::Default | Lane::Profile => 0,
    }
}

fn lane_priority(lane: Lane) -> u8 {
    match lane {
        Lane::Source => 0,
        Lane::Profile => 1,
        Lane::Default => 2,
    }
}

fn compare_lanes(state: &TagPagerState, left: Lane, right: Lane, prefix: &str) -> Ordering {
    lane_key(state, left, prefix)
        .cmp(&lane_key(state, right, prefix))
        .then_with(|| lane_priority(left).cmp(&lane_priority(right)))
        .then_with(|| lane_source_id(state, left).cmp(&lane_source_id(state, right)))
}

fn choose_lane(state: &TagPagerState, prefix: &str) -> Option<Lane> {
    [Lane::Default, Lane::Profile, Lane::Source]
        .into_iter()
        .filter(|lane| lane_key(state, *lane, prefix).is_some())
        .min_by(|left, right| compare_lanes(state, *left, *right, prefix))
}

fn lane_exact_key(state: &TagPagerState, lane: Lane, prefix: &str) -> Option<String> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset).map(String::from),
        Lane::Profile => profile_head(state, prefix).map(|entry| entry.label.clone()),
        Lane::Source => source_head(state)
            .filter(|entry| !entry.is_preview)
            .map(|entry| entry.normalized_text.clone()),
    }
}

fn lane_candidate(state: &TagPagerState, lane: Lane, prefix: &str) -> Option<MergedCandidate> {
    match lane {
        Lane::Default => default_head(prefix, state.default_offset).map(MergedCandidate::Default),
        Lane::Profile => profile_head(state, prefix).map(|entry| MergedCandidate::Profile {
            label: entry.label.clone(),
        }),
        Lane::Source => source_head(state).map(|entry| MergedCandidate::Source {
            tag_id: entry.id,
            label: source_display_label(entry),
            exact_label: (!entry.is_preview).then(|| entry.normalized_text.clone()),
            revision: state
                .source_page
                .as_ref()
                .map(|page| page.revision)
                .unwrap_or_default(),
        }),
    }
}

fn advance_lane(state: &mut TagPagerState, lane: Lane) {
    match lane {
        Lane::Default => state.default_offset = state.default_offset.saturating_add(1),
        Lane::Profile => {
            if let Some(page) = state.profile_page.as_mut() {
                page.offset = page.offset.saturating_add(1);
            }
        }
        Lane::Source => {
            if let Some(page) = state.source_page.as_mut() {
                page.offset = page.offset.saturating_add(1);
            }
        }
    }
}

fn source_display_label(entry: &SourceTagPageEntry) -> String {
    if entry.is_preview && !entry.display_label.ends_with('…') {
        format!("{}…", entry.display_label)
    } else {
        entry.display_label.clone()
    }
}

impl MergedCandidate {
    /// Return the stable identity captured by pointer callbacks.
    pub(super) fn token(&self) -> CandidateToken {
        match self {
            Self::Default(label) => CandidateToken::Default(label),
            Self::Profile { label } => CandidateToken::Profile(label.clone()),
            Self::Source { tag_id, .. } => CandidateToken::Source(*tag_id),
        }
    }

    /// Return the bounded label shown to the user.
    pub(super) fn display_label(&self) -> String {
        match self {
            Self::Default(label) => String::from(*label),
            Self::Profile { label } => label.clone(),
            Self::Source { label, .. } => label.clone(),
        }
    }

    /// Return an exact ghost suffix when the candidate has a canonical label.
    pub(super) fn ghost_suffix(&self, prefix: &str) -> Option<String> {
        let label = match self {
            Self::Default(label) => *label,
            Self::Profile { label } => label.as_str(),
            Self::Source {
                exact_label: Some(label),
                ..
            } => label.as_str(),
            Self::Source {
                exact_label: None, ..
            } => return None,
        };
        label
            .strip_prefix(prefix)
            .filter(|suffix| !suffix.is_empty())
            .map(String::from)
    }
}
