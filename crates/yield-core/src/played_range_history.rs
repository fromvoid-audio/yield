#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral history of the portions of a playback span that were heard.

use std::sync::Arc;

use crate::WaveformRegion;

const MERGE_EPSILON: f64 = 1.0e-6;

/// Sorted, disjoint normalized ranges that have been heard during playback.
///
/// The history keeps one transient progress anchor separately from the retained ranges. Starting
/// a new playback lifetime replaces that anchor without erasing the ranges. Invalid progress
/// clears only the anchor, so an incomplete sample cannot bridge a gap on a later update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayedRangeHistory {
    ranges: Arc<Vec<WaveformRegion>>,
    anchor: Option<ProgressAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ProgressAnchor {
    span: WaveformRegion,
    position: f64,
}

impl PlayedRangeHistory {
    /// Return an immutable, cheaply cloned snapshot of the retained ranges.
    pub fn snapshot(&self) -> Arc<Vec<WaveformRegion>> {
        Arc::clone(&self.ranges)
    }

    /// Return whether no heard ranges have been retained.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Start a new playback lifetime at a validated position inside `span`.
    ///
    /// The retained history is preserved while any previous progress anchor is discarded.
    /// Non-finite positions and invalid spans leave the anchor cleared and return `false`.
    pub fn start(&mut self, span: WaveformRegion, position: f64) -> bool {
        let Some(position) = normalized_position(span, position) else {
            self.anchor = None;
            return false;
        };
        self.anchor = Some(ProgressAnchor { span, position });
        true
    }

    /// Record one validated playback position and return whether retained ranges changed.
    ///
    /// Forward movement records the interval between the previous and current positions. A
    /// looping backward movement records the tail and head pieces of the span. A non-looping
    /// backward movement records no interval because the seeked-over portion was not heard.
    /// Changing the configured span resets the anchor at the current position and records no
    /// bridge across the change.
    pub fn record_progress(&mut self, span: WaveformRegion, position: f64, looping: bool) -> bool {
        let Some(current) = normalized_position(span, position) else {
            self.anchor = None;
            return false;
        };
        let Some(previous) = self.anchor else {
            self.anchor = Some(ProgressAnchor {
                span,
                position: current,
            });
            return false;
        };
        if previous.span != span {
            self.anchor = Some(ProgressAnchor {
                span,
                position: current,
            });
            return false;
        }

        let changed = if current + MERGE_EPSILON < previous.position {
            if looping {
                let first = insert_merged_range(&mut self.ranges, previous.position, span.end());
                let second = insert_merged_range(&mut self.ranges, span.start(), current);
                first || second
            } else {
                false
            }
        } else {
            insert_merged_range(&mut self.ranges, previous.position, current)
        };
        self.anchor = Some(ProgressAnchor {
            span,
            position: current,
        });
        changed
    }

    /// Clear the transient progress anchor while preserving heard ranges.
    pub fn clear_anchor(&mut self) {
        self.anchor = None;
    }

    /// Clear both the transient anchor and all retained ranges.
    pub fn clear(&mut self) {
        self.clear_anchor();
        if !self.ranges.is_empty() {
            self.ranges = Arc::new(Vec::new());
        }
    }
}

fn normalized_position(span: WaveformRegion, position: f64) -> Option<f64> {
    position
        .is_finite()
        .then_some(position.clamp(span.start(), span.end()))
}

fn insert_merged_range(ranges: &mut Arc<Vec<WaveformRegion>>, start: f64, end: f64) -> bool {
    if !start.is_finite() || !end.is_finite() || end - start <= 0.0 {
        return false;
    }
    let current = ranges.as_ref();
    let insertion = current.partition_point(|range| range.end() + MERGE_EPSILON < start);
    let mut merged_end = end;
    let mut remove_end = insertion;
    while remove_end < current.len() && current[remove_end].start() <= merged_end + MERGE_EPSILON {
        merged_end = merged_end.max(current[remove_end].end());
        remove_end += 1;
    }
    if remove_end == insertion {
        let ranges = Arc::make_mut(ranges);
        ranges.insert(
            insertion,
            WaveformRegion::new(start, end).expect("validated played range"),
        );
        return true;
    }

    let merged_start = current[insertion].start().min(start);
    let merged = WaveformRegion::new(merged_start, merged_end).expect("validated played range");
    let was_same = remove_end - insertion == 1 && current[insertion] == merged;
    if was_same {
        return false;
    }
    let ranges = Arc::make_mut(ranges);
    ranges.splice(insertion..remove_end, [merged]);
    true
}

#[cfg(test)]
mod tests {
    use super::PlayedRangeHistory;
    use crate::WaveformRegion;

    fn span(start: f64, end: f64) -> WaveformRegion {
        WaveformRegion::new(start, end).expect("valid span")
    }

    fn ranges(history: &PlayedRangeHistory) -> Vec<(f64, f64)> {
        history
            .snapshot()
            .iter()
            .map(|range| (range.start(), range.end()))
            .collect()
    }

    #[test]
    fn forward_progress_records_and_merges_touching_ranges() {
        let span = span(0.2, 0.8);
        let mut history = PlayedRangeHistory::default();
        assert!(history.start(span, 0.25));
        assert!(history.record_progress(span, 0.4, false));
        assert!(history.record_progress(span, 0.6, false));
        assert_eq!(ranges(&history), vec![(0.25, 0.6)]);
    }

    #[test]
    fn overlap_and_epsilon_touching_ranges_are_one_interval() {
        let span = span(0.0, 1.0);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.2);
        history.record_progress(span, 0.4, false);
        history.start(span, 0.4000005);
        history.record_progress(span, 0.7, false);
        history.start(span, 0.700001);
        history.record_progress(span, 0.9, false);
        assert_eq!(ranges(&history), vec![(0.2, 0.9)]);
    }

    #[test]
    fn looping_backward_progress_splits_at_span_edges() {
        let span = span(0.2, 0.8);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.7);
        assert!(history.record_progress(span, 0.3, true));
        assert_eq!(ranges(&history), vec![(0.2, 0.3), (0.7, 0.8)]);
    }

    #[test]
    fn non_looping_backward_progress_records_no_gap() {
        let span = span(0.2, 0.8);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.7);
        assert!(!history.record_progress(span, 0.3, false));
        assert!(history.is_empty());
        assert!(history.record_progress(span, 0.4, false));
        assert_eq!(ranges(&history), vec![(0.3, 0.4)]);
    }

    #[test]
    fn invalid_progress_clears_anchor_without_retaining_invalid_ranges() {
        let span = span(0.0, 1.0);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.2);
        history.record_progress(span, 0.5, false);
        assert!(!history.record_progress(span, f64::NAN, false));
        assert!(!history.record_progress(span, 0.8, false));
        assert_eq!(ranges(&history), vec![(0.2, 0.5)]);
    }

    #[test]
    fn start_and_stop_reset_anchor_without_erasing_ranges() {
        let span = span(0.0, 1.0);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.1);
        history.record_progress(span, 0.4, false);
        history.clear_anchor();
        assert!(!history.record_progress(span, 0.8, false));
        assert_eq!(ranges(&history), vec![(0.1, 0.4)]);
        assert!(history.start(span, 0.8));
        history.record_progress(span, 0.9, false);
        assert_eq!(ranges(&history), vec![(0.1, 0.4), (0.8, 0.9)]);
    }

    #[test]
    fn changing_span_resets_anchor_without_bridging_unheard_seek() {
        let first = span(0.1, 0.8);
        let second = span(0.2, 0.7);
        let mut history = PlayedRangeHistory::default();
        history.start(first, 0.2);
        history.record_progress(first, 0.4, false);
        assert!(!history.record_progress(second, 0.6, false));
        assert_eq!(ranges(&history), vec![(0.2, 0.4)]);
        assert!(history.record_progress(second, 0.65, false));
        assert_eq!(ranges(&history), vec![(0.2, 0.4), (0.6, 0.65)]);
    }

    #[test]
    fn one_range_insertion_can_bridge_multiple_sorted_ranges() {
        let span = span(0.0, 1.0);
        let mut history = PlayedRangeHistory::default();
        history.start(span, 0.1);
        history.record_progress(span, 0.2, false);
        history.start(span, 0.4);
        history.record_progress(span, 0.5, false);
        history.start(span, 0.6);
        history.record_progress(span, 0.7, false);
        history.start(span, 0.15);
        history.record_progress(span, 0.65, false);
        assert_eq!(ranges(&history), vec![(0.1, 0.7)]);
    }
}
