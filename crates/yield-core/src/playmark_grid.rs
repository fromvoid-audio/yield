//! Source-frame beat snapping for a single playmark paint gesture.

const BPM_SCALE: u32 = 1_000;

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
    /// Inclusive first and exclusive end frames.
    pub frames: (usize, usize),
    /// Number of complete beats in the span.
    pub beats: usize,
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
        // Invert the rounded boundary calculation to find the last beat that fits.
        let max_beats = (capacity as u128 * denominator + (denominator - 1) / 2) / period;
        if max_beats == 0 {
            return None;
        }
        let distance = self.anchor.abs_diff(pointer) as u128;
        let lower = (distance * denominator / period).clamp(1, max_beats);
        let upper = (lower + 1).min(max_beats);
        let boundary = |beats| (beats * period + denominator / 2) / denominator;
        let lower_frames = boundary(lower);
        let upper_frames = boundary(upper);
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                grid.snap(pointer, false),
                Some(SnappedPlaymark {
                    frames: (10_237, end),
                    beats,
                })
            );
        }
        assert_eq!(
            grid.snap(9_487, true),
            Some(SnappedPlaymark {
                frames: (9_237, 10_237),
                beats: 2,
            })
        );
        let fractional = PlaymarkBeatGrid::new(0, 100, 17, 300.0).unwrap();
        assert_eq!(fractional.snap(5, false).unwrap().frames, (0, 7));
    }

    #[test]
    fn fractional_periods_round_each_boundary_from_the_original_frame() {
        for (rate, expected) in [(44_100, 10_743_556), (48_000, 11_693_655)] {
            let grid = PlaymarkBeatGrid::new(123, 20_000_000, rate, 123.145).unwrap();
            assert_eq!(
                grid.snap(expected, false),
                Some(SnappedPlaymark {
                    frames: (123, expected),
                    beats: 500,
                })
            );
        }
    }

    #[test]
    fn only_complete_beats_fit_at_file_edges() {
        let grid = PlaymarkBeatGrid::new(1_050, 1_549, 1_000, 120.0).unwrap();
        assert_eq!(grid.snap(1_549, false), None);
        assert_eq!(
            grid.snap(0, true),
            Some(SnappedPlaymark {
                frames: (50, 1_050),
                beats: 2,
            })
        );
        let grid = PlaymarkBeatGrid::new(0, 501, 1_000, 120.0).unwrap();
        assert_eq!(grid.snap(501, false).unwrap().frames, (0, 500));
        assert_eq!(grid.snap(0, true), None);
        assert!(PlaymarkBeatGrid::new(0, 100, 0, 120.0).is_none());
        assert!(PlaymarkBeatGrid::new(0, 100, 1_000, f64::NAN).is_none());
    }
}
