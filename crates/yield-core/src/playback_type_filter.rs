#![deny(missing_docs)]
#![deny(warnings)]

//! Pure playback-type values and filtering for browser projections.

/// A recognized source playback classification.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlaybackType {
    /// A sample intended to play once.
    OneShot,
    /// A sample classified as loopable.
    Loop,
}

/// The two recognized playback types, in the order used by the browser chips.
pub const PLAYBACK_TYPE_FILTERS: [PlaybackType; 2] = [PlaybackType::OneShot, PlaybackType::Loop];

/// A validated selection of browser playback types.
///
/// Each bit corresponds to one value in [`PLAYBACK_TYPE_FILTERS`]. A zero mask disables
/// playback-type filtering and lets every row through, including rows whose type metadata is not
/// available yet.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PlaybackTypeFilter {
    mask: u8,
}

impl PlaybackTypeFilter {
    /// Construct a disabled playback-type filter.
    pub const fn disabled() -> Self {
        Self { mask: 0 }
    }

    /// Construct a filter from a two-bit type mask.
    ///
    /// Bits outside the two recognized playback types are discarded, so the resulting filter is
    /// always valid. A zero mask is the disabled filter.
    pub const fn from_mask(mask: u8) -> Self {
        Self {
            mask: mask & VALID_MASK,
        }
    }

    /// Return the selected playback-type mask.
    pub const fn mask(self) -> u8 {
        self.mask
    }

    /// Return whether no playback type is selected.
    pub const fn is_disabled(self) -> bool {
        self.mask == 0
    }

    /// Return whether at least one playback type is selected.
    pub const fn is_enabled(self) -> bool {
        self.mask != 0
    }

    /// Return whether one recognized playback type is selected.
    pub const fn contains_type(self, playback_type: PlaybackType) -> bool {
        self.mask & playback_type_bit(playback_type) != 0
    }

    /// Return whether optional playback metadata matches this filter.
    ///
    /// A disabled filter matches every row. An enabled filter excludes rows without recognized
    /// metadata rather than treating an unknown value as either known type.
    pub const fn matches(self, playback_type: Option<PlaybackType>) -> bool {
        self.is_disabled()
            || match playback_type {
                Some(playback_type) => self.contains_type(playback_type),
                None => false,
            }
    }

    /// Toggle one recognized playback type and return the resulting filter.
    pub const fn toggled_type(self, playback_type: PlaybackType) -> Self {
        Self {
            mask: self.mask ^ playback_type_bit(playback_type),
        }
    }

    /// Set one recognized playback type on or off and return the resulting filter.
    pub const fn with_type(self, playback_type: PlaybackType, enabled: bool) -> Self {
        Self {
            mask: if enabled {
                self.mask | playback_type_bit(playback_type)
            } else {
                self.mask & !playback_type_bit(playback_type)
            },
        }
    }
}

const VALID_MASK: u8 = (1 << PLAYBACK_TYPE_FILTERS.len()) - 1;

const fn playback_type_bit(playback_type: PlaybackType) -> u8 {
    match playback_type {
        PlaybackType::OneShot => 1,
        PlaybackType::Loop => 1 << 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_filter_matches_known_and_unknown_rows() {
        let filter = PlaybackTypeFilter::disabled();
        assert_eq!(filter.mask(), 0);
        assert!(filter.matches(Some(PlaybackType::OneShot)));
        assert!(filter.matches(Some(PlaybackType::Loop)));
        assert!(filter.matches(None));
    }

    #[test]
    fn active_filter_requires_recognized_metadata() {
        let one_shot = PlaybackTypeFilter::disabled().toggled_type(PlaybackType::OneShot);
        assert!(one_shot.matches(Some(PlaybackType::OneShot)));
        assert!(!one_shot.matches(Some(PlaybackType::Loop)));
        assert!(!one_shot.matches(None));
    }

    #[test]
    fn types_toggle_independently_and_clear_to_disabled() {
        let filter = PlaybackTypeFilter::disabled()
            .toggled_type(PlaybackType::OneShot)
            .toggled_type(PlaybackType::Loop);
        assert!(filter.contains_type(PlaybackType::OneShot));
        assert!(filter.contains_type(PlaybackType::Loop));
        assert_eq!(filter.mask(), 0b11);

        let filter = filter
            .toggled_type(PlaybackType::OneShot)
            .toggled_type(PlaybackType::Loop);
        assert!(filter.is_disabled());
    }

    #[test]
    fn masks_discard_unknown_bits() {
        let filter = PlaybackTypeFilter::from_mask(0b1101);
        assert_eq!(filter.mask(), 0b01);
        assert!(filter.matches(Some(PlaybackType::OneShot)));
        assert!(!filter.matches(Some(PlaybackType::Loop)));
    }
}
