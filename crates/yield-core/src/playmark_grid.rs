//! Source-frame beat snapping for a single playmark paint gesture.

const BPM_SCALE: u32 = 1_000;

/// A valid user-entered grid tempo stored in thousandths of a BPM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaymarkGridTempo(u32);

impl PlaymarkGridTempo {
    /// Parse 1–1000 BPM with up to three decimal places.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (whole, fractional) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || fractional.len() > 3
            || !whole
                .bytes()
                .chain(fractional.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let whole = whole.parse::<u32>().ok()?.checked_mul(BPM_SCALE)?;
        let fractional = if fractional.is_empty() {
            0
        } else {
            fractional.parse::<u32>().ok()? * 10_u32.pow(3 - fractional.len() as u32)
        };
        let tempo = whole.checked_add(fractional)?;
        (BPM_SCALE..=1_000 * BPM_SCALE)
            .contains(&tempo)
            .then_some(Self(tempo))
    }

    /// Convert the fixed decimal value to BPM.
    pub fn bpm(self) -> f64 {
        f64::from(self.0) / f64::from(BPM_SCALE)
    }
}

/// A fixed beat grid anchored to the frame where painting began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaymarkBeatGrid {
    anchor: usize,
    total_frames: usize,
    period_numerator: u64,
    milli_bpm: u32,
}

/// A normalized span that contains only complete beats from the gesture anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnappedPlaymark {
    frames: (usize, usize),
    beats: usize,
    grid: PlaymarkBeatGrid,
}

impl PlaymarkBeatGrid {
    /// Capture the grid. BPM is quantized once to three decimal places.
    pub fn new(anchor: usize, total_frames: usize, sample_rate: u32, bpm: f64) -> Option<Self> {
        if anchor > total_frames || total_frames == 0 || !bpm.is_finite() || bpm <= 0.0 {
            return None;
        }
        let scaled = (bpm * f64::from(BPM_SCALE)).round();
        if !(1.0..=f64::from(u32::MAX)).contains(&scaled) {
            return None;
        }
        let milli_bpm = scaled as u32;
        let period_numerator = u64::from(sample_rate) * 60 * u64::from(BPM_SCALE);
        if period_numerator < u64::from(milli_bpm) {
            return None;
        }
        Some(Self {
            anchor,
            total_frames,
            period_numerator,
            milli_bpm,
        })
    }

    /// Total source frames against which the gesture was captured.
    pub fn total_frames(self) -> usize {
        self.total_frames
    }

    fn rounded_distance(self, beats: u128) -> u128 {
        let denominator = u128::from(self.milli_bpm);
        (beats * u128::from(self.period_numerator) + denominator / 2) / denominator
    }

    fn last_boundary_within(self, distance: usize) -> u128 {
        let denominator = u128::from(self.milli_bpm);
        // Invert the rounded boundary calculation to find the last beat that fits.
        (distance as u128 * denominator + (denominator - 1) / 2) / u128::from(self.period_numerator)
    }

    /// Snap the moving edge toward the nearest whole beat, with midpoint ties away from the
    /// anchor. A tiny drag selects one beat when one fits in its direction.
    pub fn snap(self, pointer_frame: usize, backwards: bool) -> Option<SnappedPlaymark> {
        let pointer = pointer_frame.min(self.total_frames);
        let capacity = if backwards {
            self.anchor
        } else {
            self.total_frames - self.anchor
        };
        let denominator = u128::from(self.milli_bpm);
        let period = u128::from(self.period_numerator);
        let max_beats = self.last_boundary_within(capacity);
        if max_beats == 0 {
            return None;
        }
        let distance = self.anchor.abs_diff(pointer) as u128;
        let lower = (distance * denominator / period).clamp(1, max_beats);
        let upper = (lower + 1).min(max_beats);
        let lower_frames = self.rounded_distance(lower);
        let upper_frames = self.rounded_distance(upper);
        let (frames, beats) = if distance.abs_diff(upper_frames) <= distance.abs_diff(lower_frames)
        {
            (upper_frames as usize, upper)
        } else {
            (lower_frames as usize, lower)
        };
        let range = if backwards {
            (self.anchor - frames, self.anchor)
        } else {
            (self.anchor, self.anchor + frames)
        };
        Some(SnappedPlaymark {
            frames: range,
            beats: beats as usize,
            grid: self,
        })
    }
}

impl SnappedPlaymark {
    /// Inclusive first and exclusive end frames.
    pub fn frames(self) -> (usize, usize) {
        self.frames
    }

    /// Number of complete beats in the span.
    pub fn beats(self) -> usize {
        self.beats
    }

    /// The captured tempo at the grid's fixed decimal precision.
    pub fn bpm(self) -> f64 {
        f64::from(self.grid.milli_bpm) / f64::from(BPM_SCALE)
    }

    /// Total source frames against which the gesture was captured.
    pub fn total_frames(self) -> usize {
        self.grid.total_frames
    }

    /// Visible interior beat frames in order from the anchor, with a bounded draw budget.
    ///
    /// When dense, guide thinning skips beat indices without moving retained guides.
    pub fn guide_frames(
        self,
        visible: (usize, usize),
        limit: usize,
    ) -> impl Iterator<Item = usize> {
        let grid = self.grid;
        let (start, end) = self.frames;
        let first_visible = visible.0.max(start.saturating_add(1));
        let last_visible = visible.1.min(end.saturating_sub(1));
        let backwards = end == grid.anchor;
        let range = (limit > 0 && self.beats > 1 && first_visible <= last_visible).then(|| {
            let first_distance = grid.anchor.abs_diff(first_visible);
            let last_distance = grid.anchor.abs_diff(last_visible);
            let near = first_distance.min(last_distance);
            let far = first_distance.max(last_distance);
            let first = grid.last_boundary_within(near - 1) as usize + 1;
            let last = (grid.last_boundary_within(far) as usize).min(self.beats - 1);
            let count = last.saturating_add(1).saturating_sub(first);
            let step = count.div_ceil(limit).max(1);
            (first..=last).step_by(step)
        });
        range.into_iter().flatten().map(move |beat| {
            let distance = grid.rounded_distance(beat as u128) as usize;
            if backwards {
                grid.anchor - distance
            } else {
                grid.anchor + distance
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_tempo_accepts_exact_decimal_input_and_rejects_other_notation() {
        for (text, bpm) in [
            (" 120.5 ", 120.5),
            ("123.145", 123.145),
            ("1", 1.0),
            ("1000.000", 1000.0),
        ] {
            assert_eq!(
                PlaymarkGridTempo::parse(text).map(PlaymarkGridTempo::bpm),
                Some(bpm)
            );
        }
        for text in [
            "", "0", "0.999", "-2", "+120", "1000.001", "121.0001", "1e2", "NaN", "120.4.5", "١٢٠",
        ] {
            assert!(PlaymarkGridTempo::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn midpoint_ties_and_tiny_drags_snap_away_from_the_same_anchor() {
        let grid = PlaymarkBeatGrid::new(10_237, 30_000, 1_000, 120.0).unwrap();
        for (pointer, beats, end) in [
            (10_238, 1, 10_737),
            (10_986, 1, 10_737),
            (10_987, 2, 11_237),
            (10_988, 2, 11_237),
        ] {
            assert_eq!(
                grid.snap(pointer, false)
                    .map(|snap| (snap.frames(), snap.beats())),
                Some(((10_237, end), beats))
            );
        }
        assert_eq!(
            grid.snap(9_487, true)
                .map(|snap| (snap.frames(), snap.beats())),
            Some(((9_237, 10_237), 2))
        );
        let fractional = PlaymarkBeatGrid::new(0, 100, 17, 300.0).unwrap();
        assert_eq!(fractional.snap(5, false).unwrap().frames, (0, 7));
    }

    #[test]
    fn fractional_periods_round_each_boundary_from_the_original_frame() {
        for (rate, expected) in [(44_100, 10_743_556), (48_000, 11_693_655)] {
            let grid = PlaymarkBeatGrid::new(123, 20_000_000, rate, 123.145).unwrap();
            assert_eq!(
                grid.snap(expected, false)
                    .map(|snap| (snap.frames(), snap.beats())),
                Some(((123, expected), 500))
            );
        }
    }

    #[test]
    fn only_complete_beats_fit_at_file_edges() {
        let grid = PlaymarkBeatGrid::new(1_050, 1_549, 1_000, 120.0).unwrap();
        assert_eq!(grid.snap(1_549, false), None);
        assert_eq!(
            grid.snap(0, true).map(|snap| (snap.frames(), snap.beats())),
            Some(((50, 1_050), 2))
        );
        let grid = PlaymarkBeatGrid::new(0, 501, 1_000, 120.0).unwrap();
        assert_eq!(grid.snap(501, false).unwrap().frames, (0, 500));
        assert_eq!(grid.snap(0, true), None);
        assert!(PlaymarkBeatGrid::new(0, 100, 0, 120.0).is_none());
        assert!(PlaymarkBeatGrid::new(0, 100, 1_000, f64::NAN).is_none());
    }

    #[test]
    fn clipped_guides_use_the_exact_rounded_boundaries_in_both_directions() {
        let grid = PlaymarkBeatGrid::new(100, 200, 17, 300.0).unwrap();
        let forward = grid.snap(117, false).unwrap();
        let backward = grid.snap(83, true).unwrap();
        assert_eq!(
            forward.guide_frames((106, 112), 20).collect::<Vec<_>>(),
            [107, 110]
        );
        assert_eq!(
            forward.guide_frames((0, 200), 2).collect::<Vec<_>>(),
            [103, 110]
        );
        assert_eq!(
            backward.guide_frames((85, 97), 20).collect::<Vec<_>>(),
            [97, 93, 90, 86]
        );
        assert_eq!(backward.guide_frames((0, 83), 20).count(), 0);
    }

    #[test]
    fn a_dense_grid_draw_budget_does_not_scan_offscreen_beats() {
        let total = 1_000_000_000;
        let grid = PlaymarkBeatGrid::new(0, total, 1, 60.0).unwrap();
        let snap = grid.snap(total, false).unwrap();
        let visible = (800_000_000, 900_000_000);
        let guides: Vec<_> = snap.guide_frames(visible, 16).collect();
        assert_eq!(guides.len(), 16);
        assert_eq!(guides[0], visible.0);
        assert!(
            guides
                .iter()
                .all(|&frame| frame >= visible.0 && frame <= visible.1)
        );
    }
}
