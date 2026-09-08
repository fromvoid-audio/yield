#![deny(missing_docs)]
#![deny(warnings)]

//! Pure rating-level filtering for browser projections.

use crate::{MAX_RATING, RatingState};

/// The eight browser rating levels, ordered from the most negative rating to the locked top keep.
pub const RATING_FILTER_LEVELS: [i8; 8] = [-3, -2, -1, 0, 1, 2, 3, 4];

/// A validated selection of browser rating levels.
///
/// Each bit corresponds to one value in [`RATING_FILTER_LEVELS`]. A zero mask disables rating
/// filtering and lets every row through, including rows whose catalog curation metadata is not
/// available yet.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct RatingFilter {
    mask: u8,
}

impl RatingFilter {
    /// Construct a disabled rating filter.
    pub const fn disabled() -> Self {
        Self { mask: 0 }
    }

    /// Construct a filter from its complete eight-bit level mask.
    ///
    /// Every `u8` value is valid because the browser exposes exactly eight levels. A zero mask is
    /// the disabled filter.
    pub const fn from_mask(mask: u8) -> Self {
        Self { mask }
    }

    /// Return the selected level mask.
    pub const fn mask(self) -> u8 {
        self.mask
    }

    /// Return whether no rating level is selected.
    pub const fn is_disabled(self) -> bool {
        self.mask == 0
    }

    /// Return whether at least one rating level is selected.
    pub const fn is_enabled(self) -> bool {
        self.mask != 0
    }

    /// Return whether one validated browser level is selected.
    ///
    /// Invalid levels return `false` and never affect the filter.
    pub const fn contains_level(self, level: i8) -> bool {
        match level_bit(level) {
            Some(bit) => self.mask & bit != 0,
            None => false,
        }
    }

    /// Return the browser level represented by a rating and its lock state.
    ///
    /// A locked `+3` keep is exposed as level `4` (`K4`); every other state uses its signed rating
    /// value directly.
    pub const fn level_for(rating: RatingState) -> i8 {
        if rating.locked() && rating.value() == MAX_RATING {
            4
        } else {
            rating.value()
        }
    }

    /// Return whether a known rating state matches this filter.
    ///
    /// A disabled filter matches every state. Callers with optional metadata should handle a
    /// missing curation value separately; an enabled filter must not treat missing metadata as a
    /// known rating level.
    pub const fn matches(self, rating: RatingState) -> bool {
        self.is_disabled() || self.contains_level(Self::level_for(rating))
    }

    /// Toggle one browser level and return the resulting filter.
    ///
    /// Invalid levels return `None` without changing the original filter.
    pub const fn toggled_level(self, level: i8) -> Option<Self> {
        match level_bit(level) {
            Some(bit) => Some(Self {
                mask: self.mask ^ bit,
            }),
            None => None,
        }
    }

    /// Set one browser level on or off and return the resulting filter.
    ///
    /// Invalid levels return `None` without changing the original filter.
    pub const fn with_level(self, level: i8, enabled: bool) -> Option<Self> {
        match level_bit(level) {
            Some(bit) => Some(Self {
                mask: if enabled {
                    self.mask | bit
                } else {
                    self.mask & !bit
                },
            }),
            None => None,
        }
    }
}

const fn level_bit(level: i8) -> Option<u8> {
    match level {
        -3..=4 => Some(1u8 << (level + 3) as u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rating(value: i8, locked: bool) -> RatingState {
        RatingState::new(value, locked).expect("valid rating")
    }

    #[test]
    fn disabled_filter_matches_every_known_rating() {
        let filter = RatingFilter::disabled();
        assert_eq!(filter.mask(), 0);
        assert!(filter.matches(rating(-3, false)));
        assert!(filter.matches(rating(3, true)));
    }

    #[test]
    fn locked_top_keep_uses_distinct_k4_level() {
        assert_eq!(RatingFilter::level_for(rating(3, false)), 3);
        assert_eq!(RatingFilter::level_for(rating(3, true)), 4);
        assert!(RatingFilter::from_mask(1 << 7).matches(rating(3, true)));
        assert!(!RatingFilter::from_mask(1 << 6).matches(rating(3, true)));
    }

    #[test]
    fn invalid_levels_are_rejected_without_mutation() {
        let filter = RatingFilter::disabled();
        assert_eq!(filter.toggled_level(-4), None);
        assert_eq!(filter.toggled_level(5), None);
        assert_eq!(filter.with_level(5, true), None);
        assert_eq!(filter.mask(), 0);
    }

    #[test]
    fn levels_toggle_independently_and_clear_to_disabled() {
        let filter = RatingFilter::disabled()
            .toggled_level(-3)
            .expect("valid level")
            .toggled_level(2)
            .expect("valid level");
        assert!(filter.contains_level(-3));
        assert!(filter.contains_level(2));
        assert!(!filter.contains_level(-2));

        let filter = filter
            .toggled_level(-3)
            .expect("valid level")
            .toggled_level(2)
            .expect("valid level");
        assert!(filter.is_disabled());
    }

    #[test]
    fn neutral_level_is_distinct_from_unavailable_metadata() {
        let filter = RatingFilter::disabled()
            .toggled_level(0)
            .expect("valid level");
        assert!(filter.matches(rating(0, false)));
        assert!(!filter.matches(rating(1, false)));
    }
}
