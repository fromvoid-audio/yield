//! Pure, fail-closed reference Curation queue decisions for a hydrated browser row.

use crate::{AudioFile, SECONDS_PER_DAY};

/// Reference Curation queue scope selected by its dropdown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CurationQueueScope {
    /// Include every eligible row.
    #[default]
    All,
    /// Include rows whose absolute rating is at most one.
    Ratings,
    /// Include rows without complete descriptive and playback-type tags.
    Tags,
}

/// Reference queue settings, independent of the normal browser rating chips.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurationQueueMode {
    /// Whether the queue is enabled.
    pub enabled: bool,
    /// Selected Curation scope.
    pub scope: CurationQueueScope,
    /// Age in days below which an already curated row is recent.
    pub recent_ignore_days: u16,
    /// Whether locked top-keeps are included.
    pub include_locked_keep: bool,
    /// Whether recently curated rows are included.
    pub include_recent: bool,
}

impl Default for CurationQueueMode {
    fn default() -> Self {
        Self {
            enabled: false,
            scope: CurationQueueScope::All,
            recent_ignore_days: 14,
            include_locked_keep: false,
            include_recent: false,
        }
    }
}

/// Tag facts from one revision-fenced source read for a single file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CurationTagFacts {
    has_any_tag: bool,
    has_descriptive_tag: bool,
    has_playback_type_tag: bool,
}

impl CurationTagFacts {
    /// Build facts supplied by a revision-fenced per-file source read.
    pub fn from_flags(
        has_any_tag: bool,
        has_descriptive_tag: bool,
        has_playback_type_tag: bool,
    ) -> Self {
        Self {
            has_any_tag,
            has_descriptive_tag,
            has_playback_type_tag,
        }
    }
    /// Classify complete, exact source-tag labels for one file.
    ///
    /// A caller must reject truncated preview labels and fence this set to the active source
    /// revision before passing it to the queue.
    pub fn from_labels<'a>(labels: impl IntoIterator<Item = &'a str>) -> Self {
        let mut facts = Self::default();
        for label in labels {
            facts.has_any_tag = true;
            if is_playback_tag(label) {
                facts.has_playback_type_tag = true;
            } else if !label.trim().is_empty() {
                facts.has_descriptive_tag = true;
            }
        }
        facts
    }

    fn complete(self) -> bool {
        self.has_descriptive_tag && self.has_playback_type_tag
    }
}

fn is_playback_tag(label: &str) -> bool {
    let normalized = label
        .split(|ch: char| ch == '-' || ch == '_' || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    matches!(normalized.as_str(), "loop" | "oneshot" | "one shot")
}

/// Reference queue priority, from first to last.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CurationQueuePriority {
    /// Never curated, neutral, and without tags.
    NeverCuratedEmpty,
    /// Never curated, but missing a descriptive or playback-type tag.
    NeverCuratedIncomplete,
    /// Never curated with complete tags.
    NeverCuratedHasMetadata,
    /// Previously curated with an incomplete or low-rating decision.
    LowRatingIncomplete,
    /// Previously curated and no longer recent.
    Stale,
    /// Keep-two rating.
    KeepTwo,
    /// Unlocked keep-three rating.
    KeepThree,
    /// Recently curated row, when explicitly included.
    Recent,
    /// Locked top keep, when explicitly included.
    LockedKeep,
}

/// Deterministic reference queue order key for one eligible row.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CurationQueueKey {
    priority: CurationQueuePriority,
    curated_rank: i64,
    rating_rank: i8,
    name_rank: String,
}

impl CurationQueueKey {
    /// Return the reference priority used by the queue and its badges.
    pub fn priority(&self) -> CurationQueuePriority {
        self.priority
    }
}

/// Decision for one row before the queue is composed with other browser filters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CurationQueueDecision {
    /// Queue is off.
    Disabled,
    /// The catalog has not supplied authoritative rating/timestamp metadata.
    PendingMetadata,
    /// The source has not supplied revision-fenced tag facts.
    PendingTags,
    /// Complete facts prove that the row is outside the selected queue.
    Excluded,
    /// Complete facts prove that the row belongs in the queue.
    Included(CurationQueueKey),
}

/// Classify a hydrated row with reference Curation queue semantics.
///
/// The caller must fence `tags` to the active source revision. Missing metadata or tags never
/// turn into a default empty/neutral row while the queue is enabled.
pub fn curation_queue_decision(
    file: &AudioFile,
    tags: Option<CurationTagFacts>,
    mode: CurationQueueMode,
    now_epoch_seconds: i64,
) -> CurationQueueDecision {
    if !mode.enabled {
        return CurationQueueDecision::Disabled;
    }
    let Some(curation) = file.curation else {
        return CurationQueueDecision::PendingMetadata;
    };
    let Some(tags) = tags else {
        return CurationQueueDecision::PendingTags;
    };
    let rating = curation.rating();
    let locked_keep = rating.value() == 3 && rating.locked();
    if locked_keep && !mode.include_locked_keep {
        return CurationQueueDecision::Excluded;
    }
    let recent = curation.last_curated_at().is_some_and(|curated_at| {
        now_epoch_seconds.saturating_sub(curated_at)
            < i64::from(mode.recent_ignore_days) * SECONDS_PER_DAY as i64
    });
    if recent && !mode.include_recent {
        return CurationQueueDecision::Excluded;
    }
    let in_scope = match mode.scope {
        CurationQueueScope::All => true,
        CurationQueueScope::Ratings => rating.value().abs() <= 1,
        CurationQueueScope::Tags => !tags.complete(),
    };
    if !in_scope {
        return CurationQueueDecision::Excluded;
    }
    let priority = if locked_keep {
        CurationQueuePriority::LockedKeep
    } else if recent {
        CurationQueuePriority::Recent
    } else if curation.last_curated_at().is_none() {
        if rating.value() == 0 && !tags.has_any_tag {
            CurationQueuePriority::NeverCuratedEmpty
        } else if !tags.complete() {
            CurationQueuePriority::NeverCuratedIncomplete
        } else {
            CurationQueuePriority::NeverCuratedHasMetadata
        }
    } else if rating.value() == 2 {
        CurationQueuePriority::KeepTwo
    } else if rating.value() == 3 {
        CurationQueuePriority::KeepThree
    } else if !tags.complete() || rating.value() <= 1 {
        CurationQueuePriority::LowRatingIncomplete
    } else {
        CurationQueuePriority::Stale
    };
    let name_rank = file
        .path
        .file_name()
        .unwrap_or_else(|| file.path.as_os_str())
        .to_string_lossy()
        .to_ascii_lowercase();
    CurationQueueDecision::Included(CurationQueueKey {
        priority,
        curated_rank: curation.last_curated_at().unwrap_or(i64::MIN),
        rating_rank: rating.value(),
        name_rank,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFileCuration;
    use std::path::PathBuf;

    const NOW: i64 = 30 * SECONDS_PER_DAY as i64;

    fn file(name: &str, rating: i8, locked: bool, curated: Option<i64>) -> AudioFile {
        AudioFile {
            path: PathBuf::from(name),
            relative_path: PathBuf::from(name),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: AudioFileCuration::from_parts(rating, locked, 0, None, curated),
        }
    }

    fn enabled(scope: CurationQueueScope) -> CurationQueueMode {
        CurationQueueMode {
            enabled: true,
            scope,
            ..Default::default()
        }
    }

    fn facts(descriptive: bool, playback: bool) -> CurationTagFacts {
        let labels = [
            descriptive.then_some("kick"),
            playback.then_some("one-shot"),
        ];
        CurationTagFacts::from_labels(labels.into_iter().flatten())
    }

    #[test]
    fn missing_catalog_or_tag_facts_never_create_an_empty_queue_row() {
        let mut item = file("sample.wav", 0, false, None);
        let mode = enabled(CurationQueueScope::All);
        item.curation = None;
        assert_eq!(
            curation_queue_decision(&item, None, mode, NOW),
            CurationQueueDecision::PendingMetadata
        );
        item.curation = AudioFileCuration::from_parts(0, false, 0, None, None);
        assert_eq!(
            curation_queue_decision(&item, None, mode, NOW),
            CurationQueueDecision::PendingTags
        );
        assert_eq!(
            curation_queue_decision(&item, None, CurationQueueMode::default(), NOW),
            CurationQueueDecision::Disabled
        );
    }

    #[test]
    fn scopes_and_exclusions_match_reference_rules() {
        let empty = file("empty.wav", 0, false, None);
        let rated = file("rated.wav", 3, false, None);
        let locked = file("locked.wav", 3, true, None);
        let recent = file("recent.wav", 1, false, Some(NOW - SECONDS_PER_DAY as i64));
        let priority =
            |file: &AudioFile, tags, mode| match curation_queue_decision(file, tags, mode, NOW) {
                CurationQueueDecision::Included(key) => Some(key.priority()),
                _ => None,
            };
        assert_eq!(
            priority(
                &empty,
                Some(facts(false, false)),
                enabled(CurationQueueScope::All)
            ),
            Some(CurationQueuePriority::NeverCuratedEmpty)
        );
        assert_eq!(
            priority(
                &empty,
                Some(facts(false, false)),
                enabled(CurationQueueScope::Ratings)
            ),
            Some(CurationQueuePriority::NeverCuratedEmpty)
        );
        assert_eq!(
            priority(
                &rated,
                Some(facts(false, false)),
                enabled(CurationQueueScope::Ratings)
            ),
            None
        );
        assert_eq!(
            priority(
                &rated,
                Some(facts(false, false)),
                enabled(CurationQueueScope::Tags)
            ),
            Some(CurationQueuePriority::NeverCuratedIncomplete)
        );
        assert_eq!(
            priority(
                &rated,
                Some(facts(true, true)),
                enabled(CurationQueueScope::Tags)
            ),
            None
        );
        assert_eq!(
            priority(
                &locked,
                Some(facts(true, true)),
                enabled(CurationQueueScope::All)
            ),
            None
        );
        assert_eq!(
            priority(
                &recent,
                Some(facts(true, true)),
                enabled(CurationQueueScope::All)
            ),
            None
        );
        let include = CurationQueueMode {
            enabled: true,
            include_locked_keep: true,
            include_recent: true,
            ..Default::default()
        };
        assert_eq!(
            priority(&locked, Some(facts(true, true)), include),
            Some(CurationQueuePriority::LockedKeep)
        );
        assert_eq!(
            priority(&recent, Some(facts(true, true)), include),
            Some(CurationQueuePriority::Recent)
        );
    }

    #[test]
    fn queue_sort_key_preserves_priority_age_rating_and_name_order() {
        let mode = enabled(CurationQueueScope::All);
        let key =
            |file: AudioFile, tags| match curation_queue_decision(&file, Some(tags), mode, NOW) {
                CurationQueueDecision::Included(key) => key,
                other => panic!("expected included row, got {other:?}"),
            };
        let empty = key(file("a.wav", 0, false, None), facts(false, false));
        let incomplete = key(file("b.wav", 0, false, None), facts(true, false));
        let complete = key(file("c.wav", 0, false, None), facts(true, true));
        assert!(empty < incomplete && incomplete < complete);
        let older = key(file("z.wav", 0, false, Some(0)), facts(true, false));
        let newer = key(
            file("a.wav", 0, false, Some(15 * SECONDS_PER_DAY as i64)),
            facts(true, false),
        );
        assert!(older < newer);
    }

    #[test]
    fn recent_boundary_and_keep_priorities_follow_reference_order() {
        let mode = enabled(CurationQueueScope::All);
        let complete = facts(true, true);
        let at_boundary = file(
            "boundary.wav",
            1,
            false,
            Some(NOW - 14 * SECONDS_PER_DAY as i64),
        );
        let just_recent = file(
            "recent.wav",
            1,
            false,
            Some(NOW - 14 * SECONDS_PER_DAY as i64 + 1),
        );
        let priority =
            |file: &AudioFile| match curation_queue_decision(file, Some(complete), mode, NOW) {
                CurationQueueDecision::Included(key) => Some(key.priority()),
                _ => None,
            };
        assert_eq!(
            priority(&at_boundary),
            Some(CurationQueuePriority::LowRatingIncomplete)
        );
        assert_eq!(priority(&just_recent), None);
        assert_eq!(
            priority(&file("k2.wav", 2, false, Some(0))),
            Some(CurationQueuePriority::KeepTwo)
        );
        assert_eq!(
            priority(&file("k3.wav", 3, false, Some(0))),
            Some(CurationQueuePriority::KeepThree)
        );
        assert_eq!(
            priority(&file("neutral.wav", 0, false, Some(0))),
            Some(CurationQueuePriority::LowRatingIncomplete)
        );
    }

    #[test]
    fn playback_tag_spelling_does_not_count_as_descriptive_metadata() {
        for label in ["loop", "LOOP", "one_shot", "one shot", "oneshot"] {
            let facts = CurationTagFacts::from_labels([label]);
            assert!(facts.has_any_tag);
            assert!(facts.has_playback_type_tag);
            assert!(!facts.has_descriptive_tag);
        }
        let facts = CurationTagFacts::from_labels(["loop", "kick"]);
        assert!(facts.complete());
    }
}
