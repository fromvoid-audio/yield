#![deny(missing_docs)]
#![deny(warnings)]

//! Pure playback-age buckets and browser filter selection.

/// A playback-age chip in the reference browser's display order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlaybackAgeChip {
    /// No recorded playback timestamp.
    NeverPlayed,
    /// Last played at least thirty days ago.
    OlderThanMonth,
    /// Last played at least seven but less than thirty days ago.
    OlderThanWeek,
}

/// The three disjoint playback-age chips.
pub const PLAYBACK_AGE_CHIPS: [PlaybackAgeChip; 3] = [
    PlaybackAgeChip::NeverPlayed,
    PlaybackAgeChip::OlderThanMonth,
    PlaybackAgeChip::OlderThanWeek,
];

const WEEK_SECS: i64 = 7 * 24 * 60 * 60;
const MONTH_SECS: i64 = 30 * 24 * 60 * 60;
const VALID_MASK: u8 = (1 << PLAYBACK_AGE_CHIPS.len()) - 1;

/// A selection of the disjoint playback-age buckets; zero disables filtering.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PlaybackAgeFilter {
    mask: u8,
}

impl PlaybackAgeFilter {
    /// Construct a disabled filter.
    pub const fn disabled() -> Self {
        Self { mask: 0 }
    }

    /// Return whether any playback-age chip is selected.
    pub const fn is_enabled(self) -> bool {
        self.mask != 0
    }

    /// Return whether the given chip is selected.
    pub const fn contains(self, chip: PlaybackAgeChip) -> bool {
        self.mask & bit(chip) != 0
    }

    /// Toggle one chip independently of the others.
    pub const fn toggled(self, chip: PlaybackAgeChip) -> Self {
        Self {
            mask: self.mask ^ bit(chip),
        }
    }

    /// Select every other chip, or clear the filter on a repeated inversion.
    pub const fn inverted(self, chip: PlaybackAgeChip) -> Self {
        let other_mask = VALID_MASK & !bit(chip);
        Self {
            mask: if self.mask == other_mask {
                0
            } else {
                other_mask
            },
        }
    }

    /// Return whether a playback timestamp passes the current selection.
    pub fn matches(self, last_played_at: Option<i64>, now_unix_secs: i64) -> bool {
        !self.is_enabled()
            || bucket(last_played_at, now_unix_secs).is_some_and(|chip| self.contains(chip))
    }

    /// Return the next timestamp at which this row's filter membership changes.
    pub fn next_membership_change(
        self,
        last_played_at: Option<i64>,
        now_unix_secs: i64,
    ) -> Option<i64> {
        if !self.is_enabled() {
            return None;
        }
        let last_played_at = last_played_at?;
        let current = self.matches(Some(last_played_at), now_unix_secs);
        [WEEK_SECS, MONTH_SECS]
            .into_iter()
            .map(|interval| last_played_at.saturating_add(interval))
            .filter(|&at| at > now_unix_secs)
            .find(|&at| self.matches(Some(last_played_at), at) != current)
    }
}

const fn bit(chip: PlaybackAgeChip) -> u8 {
    match chip {
        PlaybackAgeChip::NeverPlayed => 1,
        PlaybackAgeChip::OlderThanMonth => 1 << 1,
        PlaybackAgeChip::OlderThanWeek => 1 << 2,
    }
}

fn bucket(last_played_at: Option<i64>, now_unix_secs: i64) -> Option<PlaybackAgeChip> {
    let last_played_at = match last_played_at {
        Some(last_played_at) => last_played_at,
        None => return Some(PlaybackAgeChip::NeverPlayed),
    };
    let age = now_unix_secs.saturating_sub(last_played_at);
    if age >= MONTH_SECS {
        Some(PlaybackAgeChip::OlderThanMonth)
    } else if age >= WEEK_SECS {
        Some(PlaybackAgeChip::OlderThanWeek)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_boundaries_and_clock_skew_match_reference() {
        let now = 90 * 24 * 60 * 60;
        assert!(
            PlaybackAgeFilter::disabled()
                .toggled(PlaybackAgeChip::NeverPlayed)
                .matches(None, now)
        );
        assert!(
            PlaybackAgeFilter::disabled()
                .toggled(PlaybackAgeChip::OlderThanMonth)
                .matches(Some(now - MONTH_SECS), now)
        );
        assert!(
            PlaybackAgeFilter::disabled()
                .toggled(PlaybackAgeChip::OlderThanWeek)
                .matches(Some(now - WEEK_SECS), now)
        );
        assert!(
            !PlaybackAgeFilter::disabled()
                .toggled(PlaybackAgeChip::OlderThanWeek)
                .matches(Some(now + WEEK_SECS), now)
        );
        assert!(
            !PlaybackAgeFilter::disabled()
                .toggled(PlaybackAgeChip::OlderThanWeek)
                .matches(Some(now - MONTH_SECS), now)
        );
    }

    #[test]
    fn independent_toggle_and_repeated_inversion() {
        let selected = PlaybackAgeFilter::disabled()
            .toggled(PlaybackAgeChip::NeverPlayed)
            .toggled(PlaybackAgeChip::OlderThanWeek);
        assert!(selected.contains(PlaybackAgeChip::NeverPlayed));
        assert!(selected.contains(PlaybackAgeChip::OlderThanWeek));
        assert!(!selected.contains(PlaybackAgeChip::OlderThanMonth));
        let inverted = selected.inverted(PlaybackAgeChip::OlderThanWeek);
        assert!(inverted.contains(PlaybackAgeChip::NeverPlayed));
        assert!(inverted.contains(PlaybackAgeChip::OlderThanMonth));
        assert!(
            !inverted
                .inverted(PlaybackAgeChip::OlderThanWeek)
                .is_enabled()
        );
    }

    #[test]
    fn next_boundary_only_when_membership_changes() {
        let filter = PlaybackAgeFilter::disabled().toggled(PlaybackAgeChip::OlderThanMonth);
        assert_eq!(filter.next_membership_change(Some(0), 0), Some(MONTH_SECS));
        let filter = PlaybackAgeFilter::disabled().toggled(PlaybackAgeChip::OlderThanWeek);
        assert_eq!(filter.next_membership_change(Some(0), 0), Some(WEEK_SECS));
        assert_eq!(
            filter.next_membership_change(Some(0), WEEK_SECS),
            Some(MONTH_SECS)
        );
        assert_eq!(filter.next_membership_change(None, 0), None);
    }
}
