#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral geometry for a non-empty waveform edit region.

/// A finite, ordered, non-empty edit region in normalized file coordinates.
///
/// Unlike [`crate::WaveformRegion`], an edit region may extend beyond the audio interval
/// `0.0..=1.0`. This preserves a selection while an extended waveform viewport exposes silence
/// margins. Construction orders finite endpoints and rejects equal endpoints or a width that
/// cannot be represented as a finite positive `f64`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformEditRegion {
    start: f64,
    end: f64,
}

impl WaveformEditRegion {
    /// Construct an ordered, non-empty edit region from normalized file coordinates.
    ///
    /// Endpoints are retained without clamping, so finite values outside `0.0..=1.0` are valid.
    /// Non-finite endpoints, equal endpoints, and endpoint pairs whose width overflows or
    /// becomes non-finite are rejected.
    pub fn new(start: f64, end: f64) -> Option<Self> {
        if !start.is_finite() || !end.is_finite() {
            return None;
        }
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let width = end - start;
        (width.is_finite() && width > 0.0).then_some(Self { start, end })
    }

    /// Return the normalized coordinate at the edit region's lower edge.
    pub fn start(self) -> f64 {
        self.start
    }

    /// Return the normalized coordinate at the edit region's upper edge.
    pub fn end(self) -> f64 {
        self.end
    }

    /// Return the finite positive normalized width of the edit region.
    pub fn width(self) -> f64 {
        self.end - self.start
    }

    /// Move this baseline by a normalized pointer delta without clamping to the audio interval.
    ///
    /// The original region is unchanged and both endpoints are translated by the same delta.
    /// Reuse the unchanged baseline for each provisional drag update. A non-finite delta or an
    /// overflowing or precision-collapsed result returns `None`.
    pub fn moved_by(self, absolute_delta: f64) -> Option<Self> {
        if !absolute_delta.is_finite() {
            return None;
        }
        let start = self.start + absolute_delta;
        let end = self.end + absolute_delta;
        Self::new(start, end)
    }

    /// Resize the lower edge from this baseline while keeping the upper edge fixed.
    ///
    /// The active edge may cross the fixed edge; the resulting region is ordered by
    /// [`Self::new`]. A non-finite coordinate, zero-width result, or overflowing width returns
    /// `None`. Reuse the unchanged baseline for each provisional drag update.
    pub fn resized_start_to(self, start: f64) -> Option<Self> {
        Self::new(start, self.end)
    }

    /// Resize the upper edge from this baseline while keeping the lower edge fixed.
    ///
    /// The active edge may cross the fixed edge; the resulting region is ordered by
    /// [`Self::new`]. A non-finite coordinate, zero-width result, or overflowing width returns
    /// `None`. Reuse the unchanged baseline for each provisional drag update.
    pub fn resized_end_to(self, end: f64) -> Option<Self> {
        Self::new(self.start, end)
    }

    /// Intersect this edit region with the normalized audio interval `0.0..=1.0`.
    ///
    /// An edit region wholly outside the audio, or one that only touches an audio boundary,
    /// produces `None`. A positive-width intersection is returned as the existing bounded
    /// [`crate::WaveformRegion`] type.
    pub fn clamped_to_audio(self) -> Option<crate::WaveformRegion> {
        let start = self.start.max(0.0);
        let end = self.end.min(1.0);
        if start >= end {
            return None;
        }
        crate::WaveformRegion::new(start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::WaveformEditRegion;
    use crate::WaveformRegion;

    #[test]
    fn construction_preserves_out_of_audio_bounds_and_orders_endpoints() {
        let region = WaveformEditRegion::new(1.4, -0.2).expect("finite non-empty region");
        assert_eq!(region.start(), -0.2);
        assert_eq!(region.end(), 1.4);
        assert!((region.width() - 1.6).abs() < 1.0e-12);
    }

    #[test]
    fn crossing_resize_reorders_and_zero_width_is_rejected() {
        let baseline = WaveformEditRegion::new(0.25, 0.75).expect("finite non-empty region");
        assert_eq!(
            baseline.resized_start_to(0.9),
            Some(WaveformEditRegion::new(0.75, 0.9).unwrap())
        );
        assert_eq!(
            baseline.resized_end_to(0.1),
            Some(WaveformEditRegion::new(0.1, 0.25).unwrap())
        );
        assert_eq!(baseline.resized_start_to(0.75), None);
        assert_eq!(baseline.resized_end_to(0.25), None);
    }

    #[test]
    fn nonfinite_endpoints_and_width_overflow_are_rejected() {
        assert_eq!(WaveformEditRegion::new(f64::NAN, 0.5), None);
        assert_eq!(WaveformEditRegion::new(0.5, f64::INFINITY), None);
        assert_eq!(WaveformEditRegion::new(-f64::MAX, f64::MAX), None);

        let baseline = WaveformEditRegion::new(0.25, 0.75).unwrap();
        assert_eq!(baseline.resized_start_to(f64::NEG_INFINITY), None);
        assert_eq!(baseline.resized_end_to(f64::NAN), None);
    }

    #[test]
    fn moving_uses_one_unchanged_baseline_and_preserves_out_of_audio_bounds() {
        let baseline = WaveformEditRegion::new(-0.2, 0.4).expect("finite non-empty region");
        let moved = baseline.moved_by(0.5).expect("finite moved region");
        assert_eq!(moved, WaveformEditRegion::new(0.3, 0.9).unwrap());
        assert!((moved.width() - baseline.width()).abs() < 1.0e-12);

        let second_update = baseline.moved_by(-0.4).expect("finite moved region");
        assert!((second_update.start() + 0.6).abs() < 1.0e-12);
        assert!((second_update.end() - 0.0).abs() < 1.0e-12);
        assert_eq!(baseline.start(), -0.2);
        assert_eq!(baseline.end(), 0.4);
        assert_eq!(baseline.moved_by(f64::NAN), None);

        let wide_baseline = WaveformEditRegion::new(f64::MAX * 0.5, f64::MAX * 0.75)
            .expect("finite non-empty region");
        assert_eq!(wide_baseline.moved_by(f64::MAX * 0.5), None);

        let narrow_baseline = WaveformEditRegion::new(0.0, f64::EPSILON).unwrap();
        assert_eq!(narrow_baseline.moved_by(2.0), None);
    }

    #[test]
    fn intersection_returns_only_positive_width_audio_content() {
        assert_eq!(
            WaveformEditRegion::new(-0.2, 0.4)
                .unwrap()
                .clamped_to_audio(),
            Some(WaveformRegion::new(0.0, 0.4).unwrap())
        );
        assert_eq!(
            WaveformEditRegion::new(0.6, 1.4)
                .unwrap()
                .clamped_to_audio(),
            Some(WaveformRegion::new(0.6, 1.0).unwrap())
        );
        assert_eq!(
            WaveformEditRegion::new(-2.0, -1.0)
                .unwrap()
                .clamped_to_audio(),
            None
        );
        assert_eq!(
            WaveformEditRegion::new(1.0, 2.0)
                .unwrap()
                .clamped_to_audio(),
            None
        );
    }
}
