//! Pure rating transition rules shared by browser frontends.

/// Lowest supported rating value.
pub const MIN_RATING: i8 = -3;

/// Highest supported rating value.
pub const MAX_RATING: i8 = 3;

/// A validated rating value and its optional keep lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RatingState {
    value: i8,
    locked: bool,
}

impl RatingState {
    /// Construct a rating state when `value` is within the supported range.
    pub fn new(value: i8, locked: bool) -> Option<Self> {
        (MIN_RATING..=MAX_RATING)
            .contains(&value)
            .then_some(Self { value, locked })
    }

    /// Return the signed rating value.
    pub const fn value(self) -> i8 {
        self.value
    }

    /// Return whether this rating is locked against further keyboard changes.
    pub const fn locked(self) -> bool {
        self.locked
    }
}

/// Result of applying one directional rating adjustment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RatingTransition {
    /// The input did not produce a change, including a zero delta or a locked state.
    Unchanged,
    /// The rating should be stored with this new state.
    Changed(RatingState),
    /// The caller should route a trash request through its filesystem transaction policy.
    RequiresTrash,
}

/// Calculate the next rating state without performing persistence or filesystem operations.
///
/// The magnitude of `delta` is ignored; only its sign selects the direction. Moving across
/// neutral skips zero (`-1` to `+1` and `+1` to `-1`). A fourth positive step at `+3` locks the
/// keep rating. A fourth negative step at `-3` returns [`RatingTransition::RequiresTrash`] so a
/// higher layer can route the trash operation through its filesystem transaction policy. Already
/// locked states are unchanged.
pub fn next_rating_state(current: RatingState, delta: i8) -> RatingTransition {
    if current.locked() || delta == 0 {
        return RatingTransition::Unchanged;
    }

    let direction = delta.signum();
    if current.value() == MAX_RATING && direction > 0 {
        return RatingTransition::Changed(
            RatingState::new(MAX_RATING, true).expect("rating bounds are valid"),
        );
    }
    if current.value() == MIN_RATING && direction < 0 {
        return RatingTransition::RequiresTrash;
    }

    let mut next_value = current.value() + direction;
    if current.value() != 0 && next_value == 0 {
        next_value += direction;
    }
    let next = RatingState::new(next_value.clamp(MIN_RATING, MAX_RATING), false)
        .expect("clamped rating is valid");
    (next != current)
        .then_some(RatingTransition::Changed(next))
        .unwrap_or(RatingTransition::Unchanged)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RATING, MIN_RATING, RatingState, RatingTransition, next_rating_state};

    fn state(value: i8, locked: bool) -> RatingState {
        RatingState::new(value, locked).expect("valid test rating")
    }

    #[test]
    fn validates_range_and_skips_neutral_when_changing_direction() {
        assert!(RatingState::new(MIN_RATING, false).is_some());
        assert!(RatingState::new(MAX_RATING, false).is_some());
        assert!(RatingState::new(MIN_RATING - 1, false).is_none());
        assert!(RatingState::new(MAX_RATING + 1, false).is_none());

        assert_eq!(
            next_rating_state(state(1, false), -1),
            RatingTransition::Changed(state(-1, false))
        );
        assert_eq!(
            next_rating_state(state(-1, false), 1),
            RatingTransition::Changed(state(1, false))
        );
    }

    #[test]
    fn locks_on_fourth_positive_step_and_skips_already_locked_state() {
        assert_eq!(
            next_rating_state(state(MAX_RATING, false), 1),
            RatingTransition::Changed(state(MAX_RATING, true))
        );
        assert_eq!(
            next_rating_state(state(MAX_RATING, true), 1),
            RatingTransition::Unchanged
        );
        assert_eq!(
            next_rating_state(state(MAX_RATING, true), -1),
            RatingTransition::Unchanged
        );
    }

    #[test]
    fn requests_trash_instead_of_performing_a_destructive_action() {
        assert_eq!(
            next_rating_state(state(MIN_RATING, false), -1),
            RatingTransition::RequiresTrash
        );
        assert_eq!(
            next_rating_state(state(MIN_RATING, false), 1),
            RatingTransition::Changed(state(-2, false))
        );
    }

    #[test]
    fn zero_delta_and_boundary_noops_are_explicit() {
        assert_eq!(
            next_rating_state(state(0, false), 0),
            RatingTransition::Unchanged
        );
        assert_eq!(
            next_rating_state(state(0, false), i8::MIN),
            RatingTransition::Changed(state(-1, false))
        );
        assert_eq!(
            next_rating_state(state(MAX_RATING, false), i8::MAX),
            RatingTransition::Changed(state(MAX_RATING, true))
        );
    }
}
