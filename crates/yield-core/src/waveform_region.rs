#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral geometry for a non-empty normalized playback region.

/// A finite, ordered, non-empty playback region in normalized file coordinates.
///
/// The endpoints always satisfy `0.0 <= start < end <= 1.0`. Construction clamps finite
/// coordinates to that interval and orders them, while non-finite or zero-width input is rejected.
/// Region movement and resizing are pure operations, so a caller can retain the original region
/// as a drag baseline and derive every provisional update from that same baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformRegion {
    start: f64,
    end: f64,
}

impl WaveformRegion {
    /// Construct an ordered non-empty region from normalized endpoint coordinates.
    ///
    /// Finite endpoints are clamped to `0.0..=1.0` and ordered. Non-finite or equal endpoints
    /// return `None`.
    pub fn new(start: f64, end: f64) -> Option<Self> {
        let start = normalize_endpoint(start)?;
        let end = normalize_endpoint(end)?;
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        (start < end).then_some(Self { start, end })
    }

    /// Return the normalized start coordinate.
    pub fn start(self) -> f64 {
        self.start
    }

    /// Return the normalized end coordinate.
    pub fn end(self) -> f64 {
        self.end
    }

    /// Return the normalized width of the region.
    pub fn width(self) -> f64 {
        self.end - self.start
    }

    /// Move this baseline by an absolute normalized pointer delta.
    ///
    /// The region width is preserved and the result is clamped to the file edges. Callers should
    /// invoke this on the unchanged drag baseline for each pointer update instead of repeatedly
    /// applying the delta to the previous provisional result. A non-finite delta returns `None`.
    pub fn moved_by(self, absolute_delta: f64) -> Option<Self> {
        if !absolute_delta.is_finite() {
            return None;
        }
        let width = self.width();
        let start = (self.start + absolute_delta).clamp(0.0, 1.0 - width);
        Self::new(start, start + width)
    }

    /// Resize the start edge from this baseline while keeping the opposite edge fixed.
    ///
    /// Crossing the fixed edge is normalized into ordered coordinates, allowing the active edge
    /// to pass through the other edge. A non-finite coordinate or a zero-width result returns
    /// `None`. Reuse the unchanged baseline for each provisional drag update.
    pub fn resized_start_to(self, start: f64) -> Option<Self> {
        let start = normalize_endpoint(start)?;
        Self::new(start, self.end)
    }

    /// Resize the end edge from this baseline while keeping the opposite edge fixed.
    ///
    /// Crossing the fixed edge is normalized into ordered coordinates, allowing the active edge
    /// to pass through the other edge. A non-finite coordinate or a zero-width result returns
    /// `None`. Reuse the unchanged baseline for each provisional drag update.
    pub fn resized_end_to(self, end: f64) -> Option<Self> {
        let end = normalize_endpoint(end)?;
        Self::new(self.start, end)
    }

    /// Slide the region one region-width toward the start of the file.
    ///
    /// The result is clamped at `0.0` and preserves the region width.
    pub fn slide_left(self) -> Self {
        let start = (self.start - self.width()).max(0.0);
        Self {
            start,
            end: start + self.width(),
        }
    }

    /// Slide the region one region-width toward the end of the file.
    ///
    /// The result is clamped at `1.0` and preserves the region width.
    pub fn slide_right(self) -> Self {
        let width = self.width();
        let start = (self.start + width).min(1.0 - width);
        Self {
            start,
            end: start + width,
        }
    }
}

fn normalize_endpoint(value: f64) -> Option<f64> {
    value.is_finite().then_some(value.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::WaveformRegion;

    #[test]
    fn construction_orders_clamps_and_rejects_invalid_regions() {
        assert_eq!(
            WaveformRegion::new(0.8, 0.2),
            Some(WaveformRegion {
                start: 0.2,
                end: 0.8,
            })
        );
        assert_eq!(
            WaveformRegion::new(-0.2, 1.2),
            Some(WaveformRegion {
                start: 0.0,
                end: 1.0,
            })
        );
        assert_eq!(WaveformRegion::new(0.5, 0.5), None);
        assert_eq!(WaveformRegion::new(f64::NAN, 0.5), None);
        assert_eq!(WaveformRegion::new(0.5, f64::INFINITY), None);
    }

    #[test]
    fn moving_from_one_baseline_preserves_width_and_clamps_edges() {
        let baseline = WaveformRegion::new(0.2, 0.4).unwrap();
        let moved = baseline.moved_by(0.3).unwrap();
        assert_eq!(moved, WaveformRegion::new(0.5, 0.7).unwrap());
        assert!((moved.width() - baseline.width()).abs() < 1.0e-12);
        assert_eq!(
            baseline.moved_by(-0.5),
            Some(WaveformRegion::new(0.0, 0.2).unwrap())
        );
        assert_eq!(
            baseline.moved_by(0.9),
            Some(WaveformRegion::new(0.8, 1.0).unwrap())
        );
        assert_eq!(baseline.moved_by(f64::NAN), None);

        // A second update derives from the same baseline rather than accumulating the first
        // pointer delta.
        let second_update = baseline.moved_by(0.1).unwrap();
        let expected_second_update = WaveformRegion::new(0.3, 0.5).unwrap();
        assert!((second_update.start() - expected_second_update.start()).abs() < 1.0e-12);
        assert!((second_update.end() - expected_second_update.end()).abs() < 1.0e-12);
        assert_eq!(baseline.start(), 0.2);
        assert_eq!(baseline.end(), 0.4);
    }

    #[test]
    fn resizing_normalizes_crossing_and_rejects_zero_width() {
        let baseline = WaveformRegion::new(0.25, 0.75).unwrap();
        assert_eq!(
            baseline.resized_start_to(0.9),
            Some(WaveformRegion::new(0.75, 0.9).unwrap())
        );
        assert_eq!(
            baseline.resized_end_to(0.1),
            Some(WaveformRegion::new(0.1, 0.25).unwrap())
        );
        assert_eq!(
            baseline.resized_start_to(-0.4),
            Some(WaveformRegion::new(0.0, 0.75).unwrap())
        );
        assert_eq!(
            baseline.resized_end_to(1.4),
            Some(WaveformRegion::new(0.25, 1.0).unwrap())
        );
        assert_eq!(baseline.resized_start_to(0.75), None);
        assert_eq!(baseline.resized_end_to(0.25), None);
        assert_eq!(baseline.resized_start_to(f64::NAN), None);
        assert_eq!(baseline.resized_end_to(f64::NEG_INFINITY), None);
    }

    #[test]
    fn sliding_moves_by_one_width_and_stops_at_file_edges() {
        let region = WaveformRegion::new(0.25, 0.5).unwrap();
        assert_eq!(region.slide_left(), WaveformRegion::new(0.0, 0.25).unwrap());
        assert_eq!(
            region.slide_right(),
            WaveformRegion::new(0.5, 0.75).unwrap()
        );

        let left_edge = WaveformRegion::new(0.0, 0.25).unwrap();
        let right_edge = WaveformRegion::new(0.75, 1.0).unwrap();
        assert_eq!(left_edge.slide_left(), left_edge);
        assert_eq!(right_edge.slide_right(), right_edge);
        assert_eq!(left_edge.slide_right().width(), left_edge.width());
        assert_eq!(right_edge.slide_left().width(), right_edge.width());
    }
}
