#![deny(missing_docs)]
#![deny(warnings)]

//! Duration and BPM label geometry and editing overlay for the waveform play selection.

use gpui::{
    Bounds, ContentMask, Entity, Hsla, MouseButton, Pixels, TextRun, canvas, div, fill, point,
    prelude::*, px, size,
};
use wavecrate_core::{WaveformViewport, playmark_selection_label};

use crate::search_input::SearchInput;

use super::waveform::WaveformPlayRegion;

/// Height reserved for the waveform playmark label.
pub(super) const PLAYMARK_LABEL_HEIGHT: f32 = 18.0;
/// Distance between the playmark label and the waveform's lower edge.
pub(super) const PLAYMARK_LABEL_BOTTOM_INSET: f32 = 6.0;
/// Minimum gap between a label placed inside its playmark and either selection edge.
pub(super) const PLAYMARK_LABEL_SELECTION_INSET: f32 = 4.0;
/// Gap used when a narrow playmark places its label outside the selection.
pub(super) const PLAYMARK_LABEL_OUTSIDE_GAP: f32 = 6.0;
/// Approximate glyph width used to reserve the compact label width.
pub(super) const PLAYMARK_LABEL_GLYPH_WIDTH: f32 = 10.0;
/// Minimum width of a playmark label.
pub(super) const PLAYMARK_LABEL_MIN_WIDTH: f32 = 80.0;
/// Maximum width of a playmark label.
pub(super) const PLAYMARK_LABEL_MAX_WIDTH: f32 = 150.0;

/// The side on which a playmark label is placed relative to its selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlaymarkLabelPlacement {
    /// The label fits inside the selected range.
    Inside,
    /// The label is immediately to the right of a narrow selection.
    OutsideRight,
    /// The label is immediately to the left of a narrow selection.
    OutsideLeft,
    /// Both outside sides are clipped, so the label is clamped to the roomier right side.
    FallbackRight,
    /// Both outside sides are clipped, so the label is clamped to the roomier left side.
    FallbackLeft,
}

/// Geometry for one rendered playmark label.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlaymarkLabelLayout {
    /// Bounds of the label background and text clip.
    pub(super) rect: Bounds<Pixels>,
    /// Relative placement selected by the native layout policy.
    pub(super) placement: PlaymarkLabelPlacement,
}

/// Return native-compatible label geometry for a selection rectangle in waveform bounds.
pub(super) fn playmark_label_layout(
    bounds: Bounds<Pixels>,
    selection_rect: Bounds<Pixels>,
    label_len: usize,
) -> Option<PlaymarkLabelLayout> {
    let bounds_left = f32::from(bounds.left());
    let bounds_top = f32::from(bounds.top());
    let bounds_width = f32::from(bounds.size.width);
    let bounds_height = f32::from(bounds.size.height);
    if !bounds_left.is_finite()
        || !bounds_top.is_finite()
        || !bounds_width.is_finite()
        || bounds_width <= 0.0
        || !bounds_height.is_finite()
        || bounds_height <= 0.0
    {
        return None;
    }

    let selection_left_value = f32::from(selection_rect.left());
    let selection_right_value = f32::from(selection_rect.right());
    if !selection_left_value.is_finite() || !selection_right_value.is_finite() {
        return None;
    }
    let bounds_right = bounds_left + bounds_width;
    let selection_left = selection_left_value.clamp(bounds_left, bounds_right);
    let selection_right = selection_right_value.clamp(selection_left, bounds_right);
    let selection_width = selection_right - selection_left;
    let desired_label_width = label_len as f32 * PLAYMARK_LABEL_GLYPH_WIDTH + 20.0;
    let label_width = desired_label_width
        .clamp(PLAYMARK_LABEL_MIN_WIDTH, PLAYMARK_LABEL_MAX_WIDTH)
        .min(bounds_width);
    let (left, placement) = if selection_width >= label_width + PLAYMARK_LABEL_SELECTION_INSET * 2.0
    {
        (
            (selection_left + (selection_width - label_width) * 0.5)
                .clamp(bounds_left, bounds_right - label_width),
            PlaymarkLabelPlacement::Inside,
        )
    } else {
        outside_label_left(
            bounds_left,
            bounds_right,
            selection_left,
            selection_right,
            label_width,
        )
    };
    let bounds_bottom = bounds_top + bounds_height;
    let bottom = (bounds_bottom - PLAYMARK_LABEL_BOTTOM_INSET).max(bounds_top);
    let top = (bottom - PLAYMARK_LABEL_HEIGHT).max(bounds_top);
    let label_rect = Bounds::from_corners(
        point(px(left), px(top)),
        point(px(left + label_width), px(bottom)),
    );
    Some(PlaymarkLabelLayout {
        rect: label_rect,
        placement,
    })
}

fn outside_label_left(
    bounds_left: f32,
    bounds_right: f32,
    selection_left: f32,
    selection_right: f32,
    width: f32,
) -> (f32, PlaymarkLabelPlacement) {
    let right = selection_right + PLAYMARK_LABEL_OUTSIDE_GAP;
    if right + width <= bounds_right {
        return (right, PlaymarkLabelPlacement::OutsideRight);
    }

    let left = selection_left - PLAYMARK_LABEL_OUTSIDE_GAP - width;
    if left >= bounds_left {
        return (left, PlaymarkLabelPlacement::OutsideLeft);
    }

    let right_room = bounds_right - selection_right;
    let left_room = selection_left - bounds_left;
    if right_room >= left_room {
        (
            right.clamp(bounds_left, bounds_right - width),
            PlaymarkLabelPlacement::FallbackRight,
        )
    } else {
        (
            left.clamp(bounds_left, bounds_right - width),
            PlaymarkLabelPlacement::FallbackLeft,
        )
    }
}

fn selection_rect(
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    region: WaveformPlayRegion,
) -> Option<Bounds<Pixels>> {
    let (visible_start, visible_end) =
        viewport.visible_range_from_absolute(region.start, region.end)?;
    let width = f32::from(bounds.size.width);
    if !width.is_finite() || width <= 0.0 {
        return None;
    }
    Some(Bounds::from_corners(
        point(
            bounds.left() + px(width * visible_start as f32),
            bounds.top(),
        ),
        point(
            bounds.left() + px(width * visible_end as f32),
            bounds.bottom(),
        ),
    ))
}

/// Return the rendered playmark label and its current waveform bounds.
pub(super) fn label_layout(
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    region: WaveformPlayRegion,
    total_frames: usize,
    sample_rate: u32,
    beat_count: Option<u8>,
) -> Option<(String, PlaymarkLabelLayout)> {
    let label = playmark_selection_label(region.core()?, total_frames, sample_rate, beat_count)?;
    let selection_rect = selection_rect(bounds, viewport, region)?;
    let layout = playmark_label_layout(bounds, selection_rect, label.len())?;
    Some((label, layout))
}

/// Convert a normalized playmark back to the frame tuple used by the timing parser.
pub(super) fn frame_range_for_region(
    region: WaveformPlayRegion,
    total_frames: usize,
) -> Option<(usize, usize)> {
    if total_frames == 0 || !region.is_valid() {
        return None;
    }
    let total = total_frames as f64;
    let start = normalized_frame(region.start * total, total_frames.saturating_sub(1), false);
    let end = normalized_frame(region.end * total, total_frames, true);
    Some((start, end.max(start.saturating_add(1)).min(total_frames)))
}

fn normalized_frame(value: f64, max_frame: usize, ceil: bool) -> usize {
    if !value.is_finite() {
        return 0;
    }
    let value = if ceil { value.ceil() } else { value.floor() };
    if value <= 0.0 {
        0
    } else if value >= max_frame as f64 {
        max_frame
    } else {
        value as usize
    }
}

/// Render the playmark duration/BPM label over the supplied waveform selection.
pub(super) fn render_overlay(
    region: WaveformPlayRegion,
    viewport: WaveformViewport,
    total_frames: usize,
    sample_rate: u32,
    beat_count: Option<u8>,
    background: Hsla,
    text_color: Hsla,
    editor_input: Option<Entity<SearchInput>>,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let Some((label, layout)) = label_layout(
                bounds,
                viewport,
                region,
                total_frames,
                sample_rate,
                beat_count,
            ) else {
                return None;
            };
            let input = editor_input.map(|input| {
                let mut element = div()
                    .id("playmark-timing-editor")
                    .w(layout.rect.size.width)
                    .h(layout.rect.size.height)
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_down(MouseButton::Right, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_down(MouseButton::Middle, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .child(input)
                    .into_any_element();
                element.layout_as_root(
                    size(
                        gpui::AvailableSpace::Definite(layout.rect.size.width),
                        gpui::AvailableSpace::Definite(layout.rect.size.height),
                    ),
                    window,
                    cx,
                );
                let _ = element.prepaint_at(layout.rect.origin, window, cx);
                element
            });
            Some(((label, layout), input))
        },
        move |_, prepaint, window, cx| {
            let Some(((label, layout), mut input)) = prepaint else {
                return;
            };
            let style = window.text_style();
            window.paint_quad(fill(layout.rect, background));
            if let Some(input) = input.as_mut() {
                input.paint(window, cx);
                return;
            }
            let run = TextRun {
                len: label.len(),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                label.clone().into(),
                style.font_size.to_pixels(window.rem_size()),
                &[run],
                None,
            );
            let text_width = f32::from(line.x_for_index(label.len()));
            let label_width = f32::from(layout.rect.size.width);
            let label_height = f32::from(layout.rect.size.height);
            let line_height = window.line_height();
            let text_left =
                f32::from(layout.rect.left()) + ((label_width - text_width) * 0.5).max(0.0);
            let text_top = f32::from(layout.rect.top())
                + ((label_height - f32::from(line_height)) * 0.5).max(0.0);
            window.with_content_mask(
                Some(ContentMask {
                    bounds: layout.rect,
                }),
                |window| {
                    line.paint(point(px(text_left), px(text_top)), line_height, window, cx)
                        .expect("playmark label text should paint");
                },
            );
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(left: f32, top: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(left), px(top)), gpui::size(px(width), px(height)))
    }

    fn selection(left: f32, right: f32) -> Bounds<Pixels> {
        Bounds::from_corners(point(px(left), px(0.0)), point(px(right), px(80.0)))
    }

    #[test]
    fn wide_playmark_contains_centered_length_label_with_inset() {
        let layout =
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(100.0, 300.0), 6)
                .expect("wide label layout");

        assert_eq!(layout.placement, PlaymarkLabelPlacement::Inside);
        assert_eq!(layout.rect.left(), px(160.0));
        assert_eq!(layout.rect.right(), px(240.0));
        assert_eq!(layout.rect.top(), px(56.0));
        assert_eq!(layout.rect.bottom(), px(74.0));
    }

    #[test]
    fn narrow_playmark_prefers_right_then_falls_back_left_at_right_edge() {
        let centered =
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(195.0, 205.0), 6)
                .expect("centered narrow layout");
        assert_eq!(centered.placement, PlaymarkLabelPlacement::OutsideRight);
        assert_eq!(centered.rect.left(), px(211.0));

        let right_edge =
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(350.0, 360.0), 6)
                .expect("right-edge narrow layout");
        assert_eq!(right_edge.placement, PlaymarkLabelPlacement::OutsideLeft);
        assert_eq!(right_edge.rect.left(), px(264.0));
        assert_eq!(right_edge.rect.right(), px(344.0));
    }

    #[test]
    fn narrow_playmark_near_left_edge_keeps_complete_label_to_right() {
        let layout = playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(10.0, 20.0), 6)
            .expect("left-edge narrow layout");

        assert_eq!(layout.placement, PlaymarkLabelPlacement::OutsideRight);
        assert_eq!(layout.rect.left(), px(26.0));
        assert_eq!(layout.rect.right(), px(106.0));
    }

    #[test]
    fn both_sides_fit_uses_stable_right_side_tie_breaker() {
        let first =
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(195.0, 205.0), 6)
                .expect("first layout");
        let second =
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(195.0, 205.0), 6)
                .expect("second layout");

        assert_eq!(first, second);
        assert_eq!(first.placement, PlaymarkLabelPlacement::OutsideRight);
    }

    #[test]
    fn narrow_viewport_fallback_stays_in_bounds_and_chooses_roomier_side() {
        let tied = playmark_label_layout(bounds(0.0, 0.0, 100.0, 80.0), selection(35.0, 65.0), 6)
            .expect("tied fallback layout");
        assert_eq!(tied.placement, PlaymarkLabelPlacement::FallbackRight);
        assert_eq!(tied.rect.left(), px(20.0));
        assert_eq!(tied.rect.right(), px(100.0));

        let left_roomier =
            playmark_label_layout(bounds(0.0, 0.0, 100.0, 80.0), selection(60.0, 80.0), 6)
                .expect("left-roomier fallback layout");
        assert_eq!(left_roomier.placement, PlaymarkLabelPlacement::FallbackLeft);
        assert_eq!(left_roomier.rect.left(), px(0.0));
        assert_eq!(left_roomier.rect.right(), px(80.0));
    }

    #[test]
    fn viewport_origin_and_tiny_height_are_clipped() {
        let layout =
            playmark_label_layout(bounds(20.0, 40.0, 100.0, 10.0), selection(10.0, 20.0), 6)
                .expect("tiny layout");
        assert!(layout.rect.left() >= px(20.0));
        assert!(layout.rect.right() <= px(120.0));
        assert!(layout.rect.top() >= px(40.0));
        assert!(layout.rect.bottom() <= px(50.0));
        assert_eq!(layout.rect.top(), px(40.0));
        assert_eq!(layout.rect.bottom(), px(44.0));
    }

    #[test]
    fn invalid_or_nonfinite_bounds_do_not_produce_a_label() {
        assert!(
            playmark_label_layout(bounds(0.0, 0.0, 0.0, 80.0), selection(0.0, 1.0), 6).is_none()
        );
        assert!(
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 0.0), selection(0.0, 1.0), 6).is_none()
        );
        assert!(
            playmark_label_layout(bounds(f32::NAN, 0.0, 400.0, 80.0), selection(0.0, 1.0), 6)
                .is_none()
        );
        assert!(
            playmark_label_layout(bounds(0.0, 0.0, 400.0, 80.0), selection(f32::NAN, 1.0), 6)
                .is_none()
        );
    }

    #[test]
    fn frame_range_normalization_matches_parser_edges() {
        let regions = [
            (WaveformPlayRegion::new(0.25, 0.5).unwrap(), (25, 50)),
            (WaveformPlayRegion::new(0.251, 0.501).unwrap(), (25, 51)),
            (WaveformPlayRegion::new(0.0, 0.001).unwrap(), (0, 1)),
            (WaveformPlayRegion::new(0.999, 1.0).unwrap(), (99, 100)),
        ];
        for (region, expected) in regions {
            assert_eq!(frame_range_for_region(region, 100), Some(expected));
        }
        assert_eq!(
            frame_range_for_region(WaveformPlayRegion::new(0.25, 0.5).unwrap(), 0),
            None
        );
    }
}
