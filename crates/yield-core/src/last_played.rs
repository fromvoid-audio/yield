#![deny(missing_docs)]
#![deny(warnings)]

//! Pure helpers for displaying and ordering catalog last-played metadata.

/// Number of seconds in one elapsed 24-hour day for the compact browser labels.
pub const SECONDS_PER_DAY: u64 = 86_400;

/// Format an optional Unix timestamp as the compact browser last-played label.
///
/// Missing metadata is rendered as `Never`. A timestamp from the last 24 hours is `Today`, and
/// older timestamps use an elapsed day count. `now_epoch_seconds` is explicit so callers and
/// tests share one clock sample and never consult the system clock from a row comparator.
pub fn format_last_played(last_played_at: Option<i64>, now_epoch_seconds: i64) -> String {
    let Some(age_seconds) = last_played_age_seconds(last_played_at, now_epoch_seconds) else {
        return String::from("Never");
    };
    match age_seconds / SECONDS_PER_DAY {
        0 => String::from("Today"),
        1 => String::from("1 day"),
        days => format!("{days} days"),
    }
}

/// Return the non-negative age of an optional Unix timestamp in seconds.
///
/// Negative epochs are clamped to the Unix epoch, while future timestamps are clamped to age
/// zero. Saturating arithmetic keeps malformed extremes bounded instead of overflowing.
pub const fn last_played_age_seconds(
    last_played_at: Option<i64>,
    now_epoch_seconds: i64,
) -> Option<u64> {
    let Some(last_played_at) = last_played_at else {
        return None;
    };
    let now = if now_epoch_seconds < 0 {
        0
    } else {
        now_epoch_seconds as u64
    };
    let played = if last_played_at < 0 {
        0
    } else {
        last_played_at as u64
    };
    Some(now.saturating_sub(played))
}

/// Return the ascending sort key for an optional last-played timestamp.
///
/// Recent timestamps have the smallest keys, and never-played rows use `u64::MAX` so they sort
/// last in ascending order. Descending order is applied by the browser's existing sort wrapper.
pub const fn last_played_sort_key(last_played_at: Option<i64>, now_epoch_seconds: i64) -> u64 {
    match last_played_age_seconds(last_played_at, now_epoch_seconds) {
        Some(age_seconds) => age_seconds,
        None => u64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn labels_are_deterministic_at_day_boundaries() {
        assert_eq!(format_last_played(None, NOW), "Never");
        assert_eq!(format_last_played(Some(NOW), NOW), "Today");
        assert_eq!(
            format_last_played(Some(NOW - SECONDS_PER_DAY as i64 + 1), NOW),
            "Today"
        );
        assert_eq!(
            format_last_played(Some(NOW - SECONDS_PER_DAY as i64), NOW),
            "1 day"
        );
        assert_eq!(
            format_last_played(Some(NOW - 17 * SECONDS_PER_DAY as i64), NOW),
            "17 days"
        );
    }

    #[test]
    fn future_negative_and_extreme_epochs_are_bounded() {
        assert_eq!(format_last_played(Some(NOW + 1), NOW), "Today");
        assert_eq!(last_played_age_seconds(Some(-1), NOW), Some(NOW as u64));
        assert_eq!(
            last_played_age_seconds(Some(i64::MIN), i64::MAX),
            Some(i64::MAX as u64)
        );
        assert_eq!(last_played_age_seconds(Some(i64::MAX), i64::MIN), Some(0));
        assert_eq!(last_played_age_seconds(Some(NOW), -1), Some(0));
    }

    #[test]
    fn never_played_rows_use_the_maximum_sort_key() {
        assert_eq!(last_played_sort_key(None, NOW), u64::MAX);
        assert!(last_played_sort_key(Some(NOW), NOW) < last_played_sort_key(None, NOW));
    }
}
