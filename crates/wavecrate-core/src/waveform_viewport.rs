#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral normalized viewport math for bounded waveform navigation.

const MIN_VISIBLE_FRAMES: usize = 256;
const SILENCE_MARGIN_VIEWPORT_FACTOR: f64 = 4.0;
const WHEEL_ZOOM_IN_FACTOR: f64 = 0.82;
const WHEEL_ZOOM_OUT_FACTOR: f64 = 1.22;
const KEYBOARD_ZOOM_FACTOR: f64 = 0.9;
const VIEW_EPSILON: f64 = 1.0e-9;

/// Normalized full-file viewport used to translate visible waveform coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformViewport {
    /// Normalized file coordinate at the visible left edge.
    pub start: f64,
    /// Normalized file coordinate at the visible right edge.
    pub end: f64,
}

impl Default for WaveformViewport {
    fn default() -> Self {
        Self::full()
    }
}

impl WaveformViewport {
    /// Return the full-file viewport.
    pub fn full() -> Self {
        Self {
            start: 0.0,
            end: 1.0,
        }
    }

    /// Return the normalized width, with a finite lower bound for malformed input.
    pub fn width(self) -> f64 {
        finite_or(self.end - self.start, VIEW_EPSILON).max(VIEW_EPSILON)
    }

    /// Return the normalized center of this viewport.
    pub fn center(self) -> f64 {
        self.start + self.width() * 0.5
    }

    /// Return whether the horizontal waveform scrollbar should be shown.
    ///
    /// Only the exact full-file viewport hides the scrollbar. A viewport expanded beyond the
    /// file remains visible so the margin state is represented even though its thumb may fill the
    /// track.
    pub fn scrollbar_visible(self) -> bool {
        self.start.is_finite() && self.end.is_finite() && (self.start != 0.0 || self.end != 1.0)
    }

    /// Return the fraction of the scrollbar track occupied by the viewport thumb.
    ///
    /// Expanded silence margins are intentionally capped at a full-track thumb, matching the
    /// reference browser's normalized scrollbar projection.
    pub fn scrollbar_visible_fraction(self) -> f64 {
        finite_or(self.end - self.start, 1.0).clamp(0.0, 1.0)
    }

    /// Return the normalized scrollbar offset, clamped to the available track travel.
    pub fn scrollbar_offset_fraction(self) -> f64 {
        let width = finite_or(self.end - self.start, 1.0);
        if !width.is_finite() || width >= 1.0 {
            return 0.0;
        }
        let free = 1.0 - width;
        if free <= VIEW_EPSILON {
            return 0.0;
        }
        (finite_or(self.start, 0.0) / free).clamp(0.0, 1.0)
    }

    /// Set the audio viewport from a normalized scrollbar offset.
    ///
    /// The current viewport is first clamped to the audio domain, discarding any silence margin.
    /// Non-finite offsets use the leading edge and finite offsets are clamped to the track.
    pub fn with_offset_fraction(self, total_frames: usize, offset_fraction: f64) -> Self {
        let viewport = self.clamp_to_audio(total_frames);
        let width = viewport.width().min(1.0);
        let free = (1.0 - width).max(0.0);
        let offset = finite_unit(offset_fraction, 0.0);
        let start = free * offset;
        Self {
            start,
            end: start + width,
        }
        .clamp_to_audio(total_frames)
    }

    fn extends_beyond_audio(self) -> bool {
        self.start < -VIEW_EPSILON || self.end > 1.0 + VIEW_EPSILON
    }

    fn clamp_to_audio(self, total_frames: usize) -> Self {
        self.clamp(total_frames, total_frames.max(1))
    }

    fn clamp_to_current_domain(self, total_frames: usize) -> Self {
        let max_visible_frames = if self.extends_beyond_audio() {
            (total_frames.max(1) as f64 * SILENCE_MARGIN_VIEWPORT_FACTOR).round() as usize
        } else {
            total_frames.max(1)
        };
        self.clamp(total_frames, max_visible_frames)
    }

    fn clamp(self, total_frames: usize, max_visible_frames: usize) -> Self {
        let total_frames = total_frames.max(1);
        let min_visible_frames = MIN_VISIBLE_FRAMES.min(total_frames).max(1);
        let max_visible_frames = max_visible_frames.max(min_visible_frames);
        let min_width = min_visible_frames as f64 / total_frames as f64;
        let max_width = max_visible_frames as f64 / total_frames as f64;
        let width = self.width().clamp(min_width, max_width);
        let (min_start, max_start) = if width >= 1.0 {
            (1.0 - width, 0.0)
        } else {
            (0.0, 1.0 - width)
        };
        let start = finite_or(self.start, 0.0).clamp(min_start, max_start);
        Self {
            start,
            end: start + width,
        }
    }

    /// Translate a visible normalized coordinate into a full-file coordinate.
    pub fn absolute_ratio_from_visible(self, visible_ratio: f64) -> f64 {
        self.start + self.width() * finite_unit(visible_ratio, 0.0)
    }

    /// Translate a full-file coordinate into a visible normalized coordinate.
    ///
    /// Returns `None` when the coordinate lies outside the current viewport.
    pub fn visible_ratio_from_absolute(self, absolute_ratio: f64) -> Option<f64> {
        if !absolute_ratio.is_finite() {
            return None;
        }
        let visible_ratio = (absolute_ratio - self.start) / self.width();
        (-VIEW_EPSILON..=1.0 + VIEW_EPSILON)
            .contains(&visible_ratio)
            .then(|| visible_ratio.clamp(0.0, 1.0))
    }

    /// Clip a full-file normalized range to the visible viewport.
    pub fn visible_range_from_absolute(
        self,
        start_ratio: f64,
        end_ratio: f64,
    ) -> Option<(f64, f64)> {
        if !start_ratio.is_finite() || !end_ratio.is_finite() {
            return None;
        }
        let left = start_ratio.min(end_ratio).max(self.start);
        let right = start_ratio.max(end_ratio).min(self.end);
        (right - left > VIEW_EPSILON).then(|| {
            (
                ((left - self.start) / self.width()).clamp(0.0, 1.0),
                ((right - self.start) / self.width()).clamp(0.0, 1.0),
            )
        })
    }

    /// Zoom around a visible coordinate, preserving that coordinate's file position.
    ///
    /// `zoom_in` uses the reference factor of `0.82`; zooming out uses `1.22`. A shift-enabled
    /// zoom out can expose up to four times the file length as silence margin. Plain zoom out
    /// stops when the current viewport already includes that margin.
    pub fn zoom_around_visible_anchor(
        self,
        total_frames: usize,
        visible_anchor: f64,
        zoom_in: bool,
        expand_silence_margin: bool,
    ) -> Self {
        let current = self.clamp_to_current_domain(total_frames);
        if !zoom_in && !expand_silence_margin && current.extends_beyond_audio() {
            return current;
        }
        let max_visible_frames = if expand_silence_margin {
            (total_frames.max(1) as f64 * SILENCE_MARGIN_VIEWPORT_FACTOR).round() as usize
        } else {
            total_frames.max(1)
        };
        let factor = if zoom_in {
            WHEEL_ZOOM_IN_FACTOR
        } else {
            WHEEL_ZOOM_OUT_FACTOR
        };
        let width = (current.width() * factor).clamp(
            MIN_VISIBLE_FRAMES.min(total_frames.max(1)) as f64 / total_frames.max(1) as f64,
            max_visible_frames.max(MIN_VISIBLE_FRAMES.min(total_frames.max(1))) as f64
                / total_frames.max(1) as f64,
        );
        let anchor_ratio = finite_unit(visible_anchor, 0.5);
        let anchor = current.absolute_ratio_from_visible(anchor_ratio);
        Self {
            start: anchor - width * anchor_ratio,
            end: anchor + width * (1.0 - anchor_ratio),
        }
        .clamp(total_frames, max_visible_frames)
    }

    /// Zoom around a full-file coordinate using the reference keyboard zoom factor.
    pub fn zoom_around_absolute_anchor(self, total_frames: usize, absolute_anchor: f64) -> Self {
        let current = self.clamp_to_current_domain(total_frames);
        let min_width =
            MIN_VISIBLE_FRAMES.min(total_frames.max(1)) as f64 / total_frames.max(1) as f64;
        let width = (current.width() * KEYBOARD_ZOOM_FACTOR).max(min_width);
        let anchor = finite_unit(absolute_anchor, current.center());
        Self {
            start: anchor - width * 0.5,
            end: anchor + width * 0.5,
        }
        .clamp(
            total_frames,
            (total_frames.max(1) as f64 * SILENCE_MARGIN_VIEWPORT_FACTOR).round() as usize,
        )
    }

    /// Frame a full-file normalized range while keeping the minimum visible frame span.
    pub fn zoom_to_region(self, total_frames: usize, start_ratio: f64, end_ratio: f64) -> Self {
        let start_ratio = finite_unit(start_ratio, 0.0);
        let end_ratio = finite_unit(end_ratio, 1.0);
        let (start_ratio, end_ratio) = if start_ratio <= end_ratio {
            (start_ratio, end_ratio)
        } else {
            (end_ratio, start_ratio)
        };
        let width = (end_ratio - start_ratio)
            .max(MIN_VISIBLE_FRAMES.min(total_frames.max(1)) as f64 / total_frames.max(1) as f64);
        let center = (start_ratio + end_ratio) * 0.5;
        Self {
            start: center - width * 0.5,
            end: center + width * 0.5,
        }
        .clamp_to_audio(total_frames)
    }

    /// Pan by a fraction of the visible width, clamped to the current domain.
    pub fn pan_by_visible_fraction(self, total_frames: usize, fraction: f64) -> Self {
        let current = self.clamp_to_current_domain(total_frames);
        let fraction = if fraction.is_finite() { fraction } else { 0.0 };
        Self {
            start: current.start + current.width() * fraction,
            end: current.end + current.width() * fraction,
        }
        .clamp(
            total_frames,
            if current.extends_beyond_audio() {
                (total_frames.max(1) as f64 * SILENCE_MARGIN_VIEWPORT_FACTOR).round() as usize
            } else {
                total_frames.max(1)
            },
        )
    }

    /// Pan using the visible pointer ratio captured at the start of a middle-button drag.
    pub fn pan_by_visible_ratio_drag(
        self,
        total_frames: usize,
        anchor_visible_ratio: f64,
        current_visible_ratio: f64,
    ) -> Self {
        let current = self.clamp_to_current_domain(total_frames);
        let delta = (finite_unit(current_visible_ratio, anchor_visible_ratio)
            - finite_unit(anchor_visible_ratio, 0.0))
            * current.width();
        Self {
            start: current.start - delta,
            end: current.end - delta,
        }
        .clamp(
            total_frames,
            if current.extends_beyond_audio() {
                (total_frames.max(1) as f64 * SILENCE_MARGIN_VIEWPORT_FACTOR).round() as usize
            } else {
                total_frames.max(1)
            },
        )
    }

    /// Compare viewport coordinates with a small tolerance suitable for event-driven redraws.
    pub fn is_effectively_equal(self, other: Self) -> bool {
        (self.start - other.start).abs() <= VIEW_EPSILON
            && (self.end - other.end).abs() <= VIEW_EPSILON
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    value.is_finite().then_some(value).unwrap_or(fallback)
}

fn finite_unit(value: f64, fallback: f64) -> f64 {
    finite_or(value, fallback).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_preserves_anchor_and_enforces_reference_minimum_span() {
        let viewport = WaveformViewport::full();
        let zoomed = viewport.zoom_around_visible_anchor(1_000, 0.25, true, true);
        assert!((zoomed.absolute_ratio_from_visible(0.25) - 0.25).abs() < 1e-12);
        assert!((zoomed.width() - WHEEL_ZOOM_IN_FACTOR).abs() < 1e-12);

        let narrow = WaveformViewport {
            start: 0.49,
            end: 0.51,
        };
        let minimum = narrow.zoom_around_visible_anchor(1_000, 0.5, true, true);
        assert!(minimum.width() >= MIN_VISIBLE_FRAMES as f64 / 1_000.0);
    }

    #[test]
    fn plain_zoom_out_stops_at_audio_and_shift_can_reveal_margin() {
        let viewport = WaveformViewport::full();
        let expanded = viewport.zoom_around_visible_anchor(1_000, 0.5, false, true);
        assert!(expanded.start < 0.0);
        assert!(expanded.end > 1.0);
        assert_eq!(
            viewport.zoom_around_visible_anchor(1_000, 0.5, false, false),
            viewport
        );
        assert_eq!(
            expanded.zoom_around_visible_anchor(1_000, 0.5, false, false),
            expanded
        );
    }

    #[test]
    fn pan_and_overlay_mapping_stay_in_file_coordinates() {
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        assert_eq!(viewport.absolute_ratio_from_visible(0.5), 0.5);
        assert_eq!(viewport.visible_ratio_from_absolute(0.5), Some(0.5));
        assert_eq!(viewport.visible_ratio_from_absolute(0.1), None);
        assert_eq!(viewport.visible_range_from_absolute(0.1, 0.2), None);
        let visible_range = viewport
            .visible_range_from_absolute(0.4, 0.8)
            .expect("partially visible region");
        assert!((visible_range.0 - 0.3).abs() < 1e-12);
        assert_eq!(visible_range.1, 1.0);

        let panned = viewport.pan_by_visible_ratio_drag(1_000, 0.5, 0.25);
        assert!((panned.start - 0.375).abs() < 1e-12);
        assert!((panned.end - 0.875).abs() < 1e-12);
    }

    #[test]
    fn scrollbar_geometry_matches_zoomed_viewport_endpoints_and_margins() {
        assert!(!WaveformViewport::full().scrollbar_visible());
        assert!(
            WaveformViewport {
                start: f64::EPSILON,
                end: 1.0,
            }
            .scrollbar_visible()
        );

        let leading = WaveformViewport {
            start: 0.0,
            end: 0.25,
        };
        assert!(leading.scrollbar_visible());
        assert_eq!(leading.scrollbar_visible_fraction(), 0.25);
        assert_eq!(leading.scrollbar_offset_fraction(), 0.0);

        let centered = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        assert_eq!(centered.scrollbar_visible_fraction(), 0.5);
        assert!((centered.scrollbar_offset_fraction() - 0.5).abs() < 1e-12);

        let trailing = WaveformViewport {
            start: 0.75,
            end: 1.0,
        };
        assert_eq!(trailing.scrollbar_offset_fraction(), 1.0);

        let margins = WaveformViewport {
            start: -0.5,
            end: 1.5,
        };
        assert!(margins.scrollbar_visible());
        assert_eq!(margins.scrollbar_visible_fraction(), 1.0);
        assert_eq!(margins.scrollbar_offset_fraction(), 0.0);
    }

    #[test]
    fn scrollbar_offset_clamps_to_audio_and_rejects_non_finite_input() {
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        assert_eq!(
            viewport.with_offset_fraction(1_000, 0.5),
            WaveformViewport {
                start: 0.25,
                end: 0.75,
            }
        );
        assert_eq!(
            viewport.with_offset_fraction(1_000, f64::NEG_INFINITY),
            WaveformViewport {
                start: 0.0,
                end: 0.5,
            }
        );
        assert_eq!(
            viewport.with_offset_fraction(1_000, 2.0),
            WaveformViewport {
                start: 0.5,
                end: 1.0,
            }
        );
        assert_eq!(
            WaveformViewport {
                start: -0.5,
                end: 1.5,
            }
            .with_offset_fraction(1_000, 0.5),
            WaveformViewport::full()
        );
        let malformed = WaveformViewport {
            start: f64::NAN,
            end: f64::INFINITY,
        };
        assert!(malformed.with_offset_fraction(1_000, 0.5).start.is_finite());
        assert!(malformed.with_offset_fraction(1_000, 0.5).end.is_finite());
    }
}
