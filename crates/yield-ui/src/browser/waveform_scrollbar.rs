#![deny(missing_docs)]
#![deny(warnings)]

//! Horizontal viewport scrollbar for the browser waveform.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    Bounds, Context, DragMoveEvent, Entity, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    Point, Render, Window, canvas, div, fill, point, prelude::*, px,
};
use yield_core::WaveformViewport;
use yield_design::Theme;

use crate::theme::color;

use super::Browser;

const MIN_THUMB_WIDTH: f32 = 12.0;
const GEOMETRY_EPSILON: f32 = f32::EPSILON;

/// The track and thumb rectangles for a rendered waveform scrollbar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformScrollbarGeometry {
    /// The full interactive scrollbar track.
    pub(super) track: Bounds<Pixels>,
    /// The thumb rectangle, including the minimum-width adjustment.
    pub(super) thumb: Bounds<Pixels>,
    travel: f32,
}

impl WaveformScrollbarGeometry {
    fn offset_for_track_pointer(self, position: Point<Pixels>) -> Option<f64> {
        let x = f32::from(position.x);
        let left = f32::from(self.track.origin.x);
        if !x.is_finite() || !left.is_finite() || !self.travel.is_finite() {
            return None;
        }
        if self.travel <= GEOMETRY_EPSILON {
            return Some(0.0);
        }
        let thumb_width = f32::from(self.thumb.size.width);
        Some(
            ((x - left - thumb_width * 0.5) / self.travel)
                .clamp(0.0, 1.0)
                .into(),
        )
    }

    fn grab_offset_for(self, position: Point<Pixels>) -> Option<f32> {
        let x = f32::from(position.x);
        let thumb_left = f32::from(self.thumb.origin.x);
        let thumb_width = f32::from(self.thumb.size.width);
        if !x.is_finite()
            || !thumb_left.is_finite()
            || !thumb_width.is_finite()
            || thumb_width <= 0.0
        {
            return None;
        }
        Some((x - thumb_left).clamp(0.0, thumb_width))
    }
}

/// Calculate the scrollbar geometry for a track and normalized waveform viewport.
pub(super) fn scrollbar_geometry(
    track: Bounds<Pixels>,
    viewport: WaveformViewport,
) -> Option<WaveformScrollbarGeometry> {
    if !viewport.scrollbar_visible() {
        return None;
    }
    let width = f32::from(track.size.width);
    let height = f32::from(track.size.height);
    let left = f32::from(track.origin.x);
    let top = f32::from(track.origin.y);
    if !width.is_finite()
        || width <= 0.0
        || !height.is_finite()
        || height <= 0.0
        || !left.is_finite()
        || !top.is_finite()
    {
        return None;
    }
    let visible_fraction = viewport.scrollbar_visible_fraction() as f32;
    let offset_fraction = viewport.scrollbar_offset_fraction() as f32;
    if !visible_fraction.is_finite() || !offset_fraction.is_finite() {
        return None;
    }
    let thumb_width = (width * visible_fraction.clamp(0.0, 1.0))
        .max(MIN_THUMB_WIDTH.min(width))
        .min(width);
    let travel = (width - thumb_width).max(0.0);
    let thumb_left = left + travel * offset_fraction.clamp(0.0, 1.0);
    if !thumb_left.is_finite() {
        return None;
    }
    let thumb = Bounds::from_corners(
        point(px(thumb_left), track.origin.y),
        point(px(thumb_left + thumb_width), track.bottom()),
    );
    Some(WaveformScrollbarGeometry {
        track,
        thumb,
        travel,
    })
}

/// State retained while the waveform scrollbar thumb owns a pointer drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformScrollbarDrag {
    track_left: f32,
    travel: f32,
    grab_offset: f32,
    total_frames: usize,
    generation: u64,
}

impl WaveformScrollbarDrag {
    fn begin(
        geometry: WaveformScrollbarGeometry,
        position: Point<Pixels>,
        total_frames: usize,
        generation: u64,
    ) -> Option<Self> {
        Some(Self {
            track_left: f32::from(geometry.track.origin.x),
            travel: geometry.travel,
            grab_offset: geometry.grab_offset_for(position)?,
            total_frames,
            generation,
        })
    }

    fn offset_for_pointer(self, position: Point<Pixels>) -> Option<f64> {
        let x = f32::from(position.x);
        if !x.is_finite() || !self.track_left.is_finite() || !self.travel.is_finite() {
            return None;
        }
        if self.travel <= GEOMETRY_EPSILON {
            return Some(0.0);
        }
        Some(
            ((x - self.track_left - self.grab_offset) / self.travel)
                .clamp(0.0, 1.0)
                .into(),
        )
    }

    fn position_for_track_position(self, track_position: Point<Pixels>) -> Option<Point<Pixels>> {
        let x = f32::from(track_position.x);
        (x.is_finite() && self.track_left.is_finite())
            .then(|| point(px(self.track_left + x), track_position.y))
    }
}

/// Marker for a scrollbar thumb drag owned by the browser.
#[derive(Clone)]
pub(super) struct WaveformScrollbarDragToken {
    browser: Entity<Browser>,
}

struct WaveformScrollbarDragGhost;

impl Render for WaveformScrollbarDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Render the waveform scrollbar, reserving no height for a full-file viewport.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let visible = browser
        .waveform
        .as_ref()
        .is_some_and(|_| browser.waveform_viewport.scrollbar_visible());
    let track_bounds = Rc::new(RefCell::new(None));
    let track_bounds_for_canvas = Rc::clone(&track_bounds);
    let drag_token = WaveformScrollbarDragToken {
        browser: cx.entity(),
    };
    let viewport = browser.waveform_viewport;
    let track_color = color(theme.colors.surface_raised);
    let dragging = browser.waveform_scrollbar_drag.is_some();
    let thumb_default_color = color(theme.colors.text_disabled);
    let thumb_hover_color = color(theme.colors.text_muted);
    let thumb_drag_color = color(theme.colors.accent);
    let mut scrollbar = div()
        .id("waveform-scrollbar")
        .relative()
        .flex_none()
        .w_full()
        .h(px(if visible {
            theme.waveform.scrollbar_height
        } else {
            0.0
        }))
        .bg(track_color);

    if visible {
        let canvas = canvas(
            move |bounds, _, _| {
                *track_bounds_for_canvas.borrow_mut() = Some(bounds);
            },
            move |bounds, _, window, _| {
                if let Some(geometry) = scrollbar_geometry(bounds, viewport) {
                    let thumb_color = if dragging {
                        thumb_drag_color
                    } else if geometry.track.contains(&window.mouse_position()) {
                        thumb_hover_color
                    } else {
                        thumb_default_color
                    };
                    window.paint_quad(fill(geometry.thumb, thumb_color));
                }
            },
        )
        .size_full();
        let drag_input = div()
            .id("waveform-scrollbar-drag-input")
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .on_drag(drag_token, |drag, track_position, _, cx| {
                // GPUI starts the active drag on the move that crosses its threshold and stops
                // propagation for that move. Apply that position here; waiting for
                // `on_drag_move` would discard a one-move drag before it can pan the viewport.
                let _ = drag.browser.update(cx, |view, cx| {
                    if view.update_waveform_scrollbar_drag_from_track_position(track_position) {
                        cx.notify();
                    }
                });
                cx.new(|_| WaveformScrollbarDragGhost)
            })
            .on_drag_move::<WaveformScrollbarDragToken>(cx.listener(
                |view, event: &DragMoveEvent<WaveformScrollbarDragToken>, _, cx| {
                    if view.update_waveform_scrollbar_drag(event.event.position, true) {
                        cx.notify();
                    }
                },
            ));
        scrollbar = scrollbar
            .track_focus(&browser.waveform_focus_handle)
            .cursor_pointer()
            .child(canvas)
            .child(drag_input)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    let bounds = track_bounds.borrow().as_ref().copied();
                    if let Some(bounds) = bounds {
                        if view.begin_waveform_scrollbar_pointer(event.position, bounds, window) {
                            cx.notify();
                            // The drag input has already recorded this press during bubble
                            // dispatch. Stop overlapping waveform hitboxes from also beginning
                            // a selection drag.
                            cx.stop_propagation();
                        }
                    }
                }),
            )
            .on_hover(cx.listener(|_, _: &bool, _, cx| cx.notify()))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    if view.finish_waveform_scrollbar_drag(event.position) {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    if view.finish_waveform_scrollbar_drag(event.position) {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }),
            );
    }
    scrollbar
}

impl Browser {
    pub(super) fn begin_waveform_scrollbar_pointer(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> bool {
        let Some(preview) = self.waveform.as_ref() else {
            return false;
        };
        let Some(geometry) = scrollbar_geometry(bounds, self.waveform_viewport) else {
            return false;
        };
        if !geometry.track.contains(&position) {
            return false;
        }
        window.focus(&self.waveform_focus_handle);
        if geometry.thumb.contains(&position) {
            self.waveform_scrollbar_drag = WaveformScrollbarDrag::begin(
                geometry,
                position,
                preview.total_frames,
                self.waveform_generation,
            );
        } else if let Some(offset) = geometry.offset_for_track_pointer(position) {
            self.set_waveform_scrollbar_offset(offset, preview.total_frames);
        }
        true
    }

    pub(super) fn update_waveform_scrollbar_drag(
        &mut self,
        position: Point<Pixels>,
        left_button_pressed: bool,
    ) -> bool {
        if !left_button_pressed {
            return false;
        }
        let Some(drag) = self.waveform_scrollbar_drag else {
            return false;
        };
        if !scrollbar_drag_is_current(drag, self.waveform_generation, self.waveform.is_some()) {
            self.waveform_scrollbar_drag = None;
            return false;
        }
        let Some(offset) = drag.offset_for_pointer(position) else {
            return false;
        };
        self.set_waveform_scrollbar_offset(offset, drag.total_frames)
    }

    fn update_waveform_scrollbar_drag_from_track_position(
        &mut self,
        track_position: Point<Pixels>,
    ) -> bool {
        let Some(drag) = self.waveform_scrollbar_drag else {
            return false;
        };
        let Some(position) = drag.position_for_track_position(track_position) else {
            return false;
        };
        self.update_waveform_scrollbar_drag(position, true)
    }

    pub(super) fn finish_waveform_scrollbar_drag(&mut self, position: Point<Pixels>) -> bool {
        let had_drag = self.waveform_scrollbar_drag.is_some();
        let changed = self.update_waveform_scrollbar_drag(position, true);
        self.waveform_scrollbar_drag.take();
        had_drag || changed
    }

    pub(super) fn cancel_waveform_scrollbar_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.waveform_scrollbar_drag.take().is_none() {
            return false;
        }
        cx.stop_active_drag(window);
        true
    }

    fn set_waveform_scrollbar_offset(&mut self, offset: f64, total_frames: usize) -> bool {
        let next = self
            .waveform_viewport
            .with_offset_fraction(total_frames, offset);
        if self.waveform_viewport.is_effectively_equal(next) {
            return false;
        }
        self.waveform_viewport = next;
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        true
    }
}

fn scrollbar_drag_is_current(
    drag: WaveformScrollbarDrag,
    generation: u64,
    waveform_loaded: bool,
) -> bool {
    waveform_loaded && drag.generation == generation
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(10.0), px(20.0)), size(px(width), px(height)))
    }

    #[test]
    fn geometry_reaches_track_endpoints_and_enforces_minimum_thumb() {
        let track = bounds(100.0, 6.0);
        let leading = scrollbar_geometry(
            track,
            WaveformViewport {
                start: 0.0,
                end: 0.1,
            },
        )
        .expect("zoomed viewport has a scrollbar");
        assert_eq!(leading.thumb.origin.x, px(10.0));
        assert_eq!(leading.thumb.size.width, px(12.0));

        let trailing = scrollbar_geometry(
            track,
            WaveformViewport {
                start: 0.9,
                end: 1.0,
            },
        )
        .expect("zoomed viewport has a scrollbar");
        assert_eq!(trailing.thumb.right(), px(110.0));
        assert_eq!(trailing.thumb.size.width, px(12.0));
    }

    #[test]
    fn full_viewport_hides_but_expanded_margin_keeps_full_thumb_visible() {
        assert!(scrollbar_geometry(bounds(100.0, 6.0), WaveformViewport::full()).is_none());
        let geometry = scrollbar_geometry(
            bounds(100.0, 6.0),
            WaveformViewport {
                start: -0.5,
                end: 1.5,
            },
        )
        .expect("expanded margins keep the scrollbar visible");
        assert_eq!(geometry.thumb, geometry.track);
    }

    #[test]
    fn geometry_rejects_non_finite_or_empty_tracks() {
        assert!(
            scrollbar_geometry(
                bounds(0.0, 6.0),
                WaveformViewport {
                    start: 0.25,
                    end: 0.75,
                },
            )
            .is_none()
        );
        let malformed = Bounds::new(point(px(f32::NAN), px(20.0)), size(px(100.0), px(6.0)));
        assert!(
            scrollbar_geometry(
                malformed,
                WaveformViewport {
                    start: 0.25,
                    end: 0.75,
                },
            )
            .is_none()
        );
    }

    #[test]
    fn track_click_centers_thumb_and_drag_keeps_pointer_grab_offset() {
        let geometry = scrollbar_geometry(
            bounds(100.0, 6.0),
            WaveformViewport {
                start: 0.25,
                end: 0.75,
            },
        )
        .expect("zoomed viewport has a scrollbar");
        let offset = geometry
            .offset_for_track_pointer(point(px(60.0), px(23.0)))
            .expect("finite pointer");
        assert!((offset - 0.5).abs() < f64::EPSILON);
        let drag = WaveformScrollbarDrag::begin(geometry, point(px(35.0), px(23.0)), 1_000, 7)
            .expect("thumb pointer is finite");
        assert_eq!(drag.generation, 7);
        assert!(drag.offset_for_pointer(point(px(35.0), px(23.0))).is_some());
        assert!(
            (drag
                .position_for_track_position(point(px(35.0), px(3.0)))
                .and_then(|position| drag.offset_for_pointer(position))
                .expect("finite track-relative pointer")
                - 0.7)
                .abs()
                < 1e-6
        );
        assert!(
            drag.offset_for_pointer(point(px(f32::NAN), px(23.0)))
                .is_none()
        );
    }

    #[test]
    fn stale_drag_state_is_rejected_by_generation_or_missing_preview() {
        let geometry = scrollbar_geometry(
            bounds(100.0, 6.0),
            WaveformViewport {
                start: 0.25,
                end: 0.75,
            },
        )
        .expect("zoomed viewport has a scrollbar");
        let drag = WaveformScrollbarDrag::begin(geometry, point(px(35.0), px(23.0)), 1_000, 7)
            .expect("thumb pointer is finite");
        assert!(!scrollbar_drag_is_current(drag, 8, true));
        assert!(!scrollbar_drag_is_current(drag, 7, false));
        assert!(scrollbar_drag_is_current(drag, 7, true));
        assert!(drag.offset_for_pointer(point(px(50.0), px(23.0))).is_some());
    }
}
