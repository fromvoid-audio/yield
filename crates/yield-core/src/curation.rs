//! Pure per-file curation action planning for browser frontends.
//!
//! The planner operates on already-hydrated metadata. It does not persist a result, assign a
//! timestamp, mutate a source file, or coordinate multiple rows.

use crate::{
    AudioFileCuration, COLLECTION_COUNT, RatingState, RatingTransition, next_rating_state,
};

/// One curation action requested for one browser row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurationAction {
    /// Move the row's rating in the direction indicated by `delta`.
    ///
    /// Only the sign of a non-zero delta selects the direction, matching
    /// [`next_rating_state`]. A zero delta produces [`CurationTransition::Unchanged`].
    AdjustRating {
        /// Signed direction of the rating adjustment.
        delta: i8,
    },
    /// Toggle membership in one of the fixed collection slots.
    ToggleCollection {
        /// Zero-based collection slot to toggle.
        slot: usize,
    },
    /// Remove membership from one fixed collection slot when it is present.
    RemoveCollection {
        /// Zero-based collection slot to remove.
        slot: usize,
    },
    /// Record the accepted playback time for this row.
    RecordPlayback {
        /// Unix epoch seconds captured when playback was accepted.
        played_at: i64,
    },
    /// Unlock a locked maximum keep rating.
    UnlockKeepRating,
}

/// Pure outcome of planning one curation action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurationTransition {
    /// The action is invalid or has no effect on the supplied metadata.
    Unchanged,
    /// The caller may persist this complete replacement metadata value.
    Changed(AudioFileCuration),
    /// The rating crossed the negative threshold and requires a filesystem trash transaction.
    RequiresTrash,
}

/// Plan one curation action against known browser metadata.
///
/// This function is renderer- and persistence-independent. A changed result contains the full
/// metadata value, including unrelated fields and both timestamps, so the persistence layer can
/// supply the timestamp policy when it commits the action. Invalid collection slots and zero
/// rating deltas return [`CurationTransition::Unchanged`] without deriving a replacement.
pub fn plan_curation_action(
    current: AudioFileCuration,
    action: CurationAction,
) -> CurationTransition {
    match action {
        CurationAction::AdjustRating { delta } => {
            match next_rating_state(current.rating(), delta) {
                RatingTransition::Unchanged => CurationTransition::Unchanged,
                RatingTransition::Changed(rating) => {
                    CurationTransition::Changed(with_rating(current, rating))
                }
                RatingTransition::RequiresTrash => CurationTransition::RequiresTrash,
            }
        }
        CurationAction::ToggleCollection { slot } => {
            if slot >= COLLECTION_COUNT {
                return CurationTransition::Unchanged;
            }
            CurationTransition::Changed(with_collection_mask(
                current,
                current.collection_mask() ^ (1 << slot),
            ))
        }
        CurationAction::RemoveCollection { slot } => {
            if slot >= COLLECTION_COUNT || !current.has_collection(slot) {
                return CurationTransition::Unchanged;
            }
            CurationTransition::Changed(with_collection_mask(
                current,
                current.collection_mask() & !(1 << slot),
            ))
        }
        CurationAction::RecordPlayback { played_at } => {
            if current.last_played_at() == Some(played_at) {
                return CurationTransition::Unchanged;
            }
            CurationTransition::Changed(with_last_played(current, played_at))
        }
        CurationAction::UnlockKeepRating => {
            if current.rating().value() != crate::MAX_RATING || !current.rating().locked() {
                return CurationTransition::Unchanged;
            }
            let unlocked = RatingState::new(crate::MAX_RATING, false)
                .expect("maximum rating is within the supported range");
            CurationTransition::Changed(with_rating(current, unlocked))
        }
    }
}

fn with_rating(current: AudioFileCuration, rating: RatingState) -> AudioFileCuration {
    AudioFileCuration::new(
        rating,
        current.collection_mask(),
        current.last_played_at(),
        current.last_curated_at(),
    )
    .expect("the current collection mask is valid")
}

fn with_collection_mask(current: AudioFileCuration, collection_mask: u8) -> AudioFileCuration {
    AudioFileCuration::new(
        current.rating(),
        collection_mask,
        current.last_played_at(),
        current.last_curated_at(),
    )
    .expect("toggling a supported collection slot keeps the mask valid")
}

fn with_last_played(current: AudioFileCuration, played_at: i64) -> AudioFileCuration {
    AudioFileCuration::new(
        current.rating(),
        current.collection_mask(),
        Some(played_at),
        current.last_curated_at(),
    )
    .expect("the current collection mask is valid")
}

#[cfg(test)]
mod tests {
    use super::{CurationAction, CurationTransition, plan_curation_action};
    use crate::{AudioFileCuration, COLLECTION_COUNT, MAX_RATING, MIN_RATING, RatingState};

    fn state(value: i8, locked: bool) -> RatingState {
        RatingState::new(value, locked).expect("valid test rating")
    }

    fn curation(value: i8, locked: bool, collection_mask: u8) -> AudioFileCuration {
        AudioFileCuration::new(state(value, locked), collection_mask, Some(11), Some(22))
            .expect("valid test curation")
    }

    #[test]
    fn rating_change_preserves_collections_and_curation_timestamps() {
        let current = curation(2, false, 0b0010_0101);
        let CurationTransition::Changed(after) =
            plan_curation_action(current, CurationAction::AdjustRating { delta: -1 })
        else {
            panic!("rating should change");
        };

        assert_eq!(after.rating(), state(1, false));
        assert_eq!(after.collection_mask(), current.collection_mask());
        assert_eq!(after.last_played_at(), current.last_played_at());
        assert_eq!(after.last_curated_at(), current.last_curated_at());
    }

    #[test]
    fn collection_toggle_is_independent_for_each_row() {
        let without_slot = curation(1, false, 0b0000_0001);
        let with_slot = curation(-1, false, 0b0000_0101);

        assert_eq!(
            plan_curation_action(without_slot, CurationAction::ToggleCollection { slot: 2 },),
            CurationTransition::Changed(curation(1, false, 0b0000_0101))
        );
        assert_eq!(
            plan_curation_action(with_slot, CurationAction::ToggleCollection { slot: 2 }),
            CurationTransition::Changed(curation(-1, false, 0b0000_0001))
        );
    }

    #[test]
    fn locked_rating_ignores_rating_action_but_allows_collection_toggle() {
        let current = curation(MAX_RATING, true, 0b0000_0001);

        assert_eq!(
            plan_curation_action(current, CurationAction::AdjustRating { delta: -1 }),
            CurationTransition::Unchanged
        );
        assert_eq!(
            plan_curation_action(current, CurationAction::ToggleCollection { slot: 1 }),
            CurationTransition::Changed(curation(MAX_RATING, true, 0b0000_0011))
        );
    }

    #[test]
    fn unlock_changes_only_locked_maximum_keep_rating() {
        assert_eq!(
            plan_curation_action(
                curation(MAX_RATING, true, 0b0000_0011),
                CurationAction::UnlockKeepRating,
            ),
            CurationTransition::Changed(curation(MAX_RATING, false, 0b0000_0011))
        );
        assert_eq!(
            plan_curation_action(
                curation(MAX_RATING, false, 0b0000_0011),
                CurationAction::UnlockKeepRating,
            ),
            CurationTransition::Unchanged
        );
        assert_eq!(
            plan_curation_action(
                curation(1, true, 0b0000_0011),
                CurationAction::UnlockKeepRating,
            ),
            CurationTransition::Unchanged
        );
    }

    #[test]
    fn invalid_slot_and_zero_delta_leave_metadata_unchanged() {
        let current = curation(0, false, 0b0000_0011);

        assert_eq!(
            plan_curation_action(
                current,
                CurationAction::ToggleCollection {
                    slot: COLLECTION_COUNT,
                },
            ),
            CurationTransition::Unchanged
        );
        assert_eq!(
            plan_curation_action(current, CurationAction::AdjustRating { delta: 0 }),
            CurationTransition::Unchanged
        );
        assert_eq!(current, curation(0, false, 0b0000_0011));
    }

    #[test]
    fn remove_collection_is_idempotent_and_preserves_other_memberships() {
        let current = curation(0, false, 0b0000_0101);
        assert_eq!(
            plan_curation_action(current, CurationAction::RemoveCollection { slot: 2 }),
            CurationTransition::Changed(curation(0, false, 0b0000_0001))
        );
        assert_eq!(
            plan_curation_action(
                curation(0, false, 0b0000_0001),
                CurationAction::RemoveCollection { slot: 2 },
            ),
            CurationTransition::Unchanged
        );
        assert_eq!(
            plan_curation_action(
                current,
                CurationAction::RemoveCollection {
                    slot: COLLECTION_COUNT,
                },
            ),
            CurationTransition::Unchanged
        );
    }

    #[test]
    fn negative_threshold_delegates_trash_requirement_without_io() {
        assert_eq!(
            plan_curation_action(
                curation(MIN_RATING, false, 0),
                CurationAction::AdjustRating { delta: -1 },
            ),
            CurationTransition::RequiresTrash
        );
    }

    #[test]
    fn recording_playback_preserves_curation_and_is_idempotent() {
        let current = curation(2, true, 0b0010_0101);
        let CurationTransition::Changed(after) =
            plan_curation_action(current, CurationAction::RecordPlayback { played_at: 33 })
        else {
            panic!("new playback time should change metadata");
        };
        assert_eq!(after.rating(), current.rating());
        assert_eq!(after.collection_mask(), current.collection_mask());
        assert_eq!(after.last_played_at(), Some(33));
        assert_eq!(after.last_curated_at(), current.last_curated_at());
        assert_eq!(
            plan_curation_action(after, CurationAction::RecordPlayback { played_at: 33 }),
            CurationTransition::Unchanged
        );
    }
}
