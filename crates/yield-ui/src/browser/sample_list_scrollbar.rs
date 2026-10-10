//! Visible scrollbar for the virtualized sample list.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    Bounds, Context, DragMoveEvent, Entity, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    Point, Render, Window, canvas, div, fill, point, prelude::*, px,
};
use yield_design::Theme;

use crate::theme::color;

use super::Browser;

const MIN_THUMB_HEIGHT: f32 = 28.0;
const TRACK_WIDTH: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollbarGeometry {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
    max_offset: Pixels,
}

/// Pointer offset retained while dragging the sample-list scrollbar thumb.
#[derive(Clone, Copy, Debug)]
pub(super) struct DragState {
    grab_offset: Pixels,
}

#[derive(Clone)]
struct DragToken {
    browser: Entity<Browser>,
}

struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn geometry(
    bounds: Bounds<Pixels>,
    content_height: Pixels,
    scroll_offset: Pixels,
) -> Option<ScrollbarGeometry> {
    let viewport_height = bounds.size.height;
    if viewport_height <= px(0.0) || content_height <= viewport_height {
        return None;
    }

    let max_offset = content_height - viewport_height;
    let thumb_height = (viewport_height * (viewport_height / content_height))
        .max(px(MIN_THUMB_HEIGHT))
        .min(viewport_height);
    let travel = viewport_height - thumb_height;
    let scroll_fraction = (-scroll_offset / max_offset).clamp(0.0, 1.0);
    let thumb_top = travel * scroll_fraction;
    Some(ScrollbarGeometry {
        track: bounds,
        thumb: Bounds {
            origin: point(bounds.origin.x, bounds.origin.y + thumb_top),
            size: gpui::size(bounds.size.width, thumb_height),
        },
        max_offset,
    })
}

fn offset_for_pointer(geometry: ScrollbarGeometry, y: Pixels, grab_offset: Pixels) -> Pixels {
    let travel = (geometry.track.size.height - geometry.thumb.size.height).max(px(0.0));
    if travel <= px(0.0) {
        return px(0.0);
    }
    let thumb_top = (y - geometry.track.origin.y - grab_offset).clamp(px(0.0), travel);
    -(geometry.max_offset * (thumb_top / travel))
}

/// Render a right-edge overlay for the sample list without changing table column widths.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    header_height: f32,
    has_rows: bool,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let handle = browser.list_scroll_handle.clone();
    let track_bounds = Rc::new(RefCell::new(None));
    let bounds_for_canvas = Rc::clone(&track_bounds);
    let handle_for_canvas = handle.clone();
    let has_rows_for_canvas = has_rows;
    let is_dragging = browser.sample_list_scrollbar_drag.is_some();
    let thumb_color = color(if is_dragging {
        theme.colors.accent
    } else {
        theme.colors.text_disabled
    });
    let drag_token = DragToken {
        browser: cx.entity(),
    };
    let mut input = div()
        .id("sample-list-scrollbar")
        .debug_selector(|| "sample-list-scrollbar".into())
        .absolute()
        .top(px(header_height))
        .bottom_0()
        .right_0()
        .w(px(TRACK_WIDTH))
        .cursor_pointer()
        .child(
            canvas(
                move |bounds, _, _| {
                    *bounds_for_canvas.borrow_mut() = Some(bounds);
                },
                move |bounds, _, window, _| {
                    if !has_rows_for_canvas {
                        return;
                    }
                    let scroll_state = handle_for_canvas.0.borrow();
                    let content_height = scroll_state
                        .last_item_size
                        .map_or(px(0.0), |size| size.contents.height);
                    if let Some(geometry) =
                        geometry(bounds, content_height, scroll_state.base_handle.offset().y)
                    {
                        let thumb = geometry.thumb;
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(thumb.origin.x + px(4.0), thumb.origin.y),
                                size: gpui::size(px(6.0), thumb.size.height),
                            },
                            thumb_color,
                        ));
                    }
                },
            )
            .size_full(),
        )
        .on_drag(drag_token, |drag, track_position, _, cx| {
            let _ = drag.browser.update(cx, |view, cx| {
                if let Some(bounds) = view.sample_list_scrollbar_track_bounds {
                    let position = point(
                        bounds.origin.x + track_position.x,
                        bounds.origin.y + track_position.y,
                    );
                    if view.update_sample_list_scrollbar_drag(position) {
                        cx.notify();
                    }
                }
            });
            cx.new(|_| DragGhost)
        })
        .on_drag_move::<DragToken>(
            cx.listener(|view, event: &DragMoveEvent<DragToken>, _, cx| {
                if view.update_sample_list_scrollbar_drag(event.event.position) {
                    cx.notify();
                }
            }),
        );
    input = input
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                if let Some(bounds) = *track_bounds.borrow()
                    && view.begin_sample_list_scrollbar_drag(event.position, bounds)
                {
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, _, cx| {
                if view.finish_sample_list_scrollbar_drag(event.position) {
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, _, cx| {
                if view.finish_sample_list_scrollbar_drag(event.position) {
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
        );
    input
}

impl Browser {
    fn begin_sample_list_scrollbar_drag(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> bool {
        if self.view_model.rows().is_empty() {
            return false;
        }
        let content_height = self
            .list_scroll_handle
            .0
            .borrow()
            .last_item_size
            .map_or(px(0.0), |size| size.contents.height);
        let scroll_offset = self.list_scroll_handle.0.borrow().base_handle.offset().y;
        let Some(geometry) = geometry(bounds, content_height, scroll_offset) else {
            return false;
        };
        if !geometry.track.contains(&position) {
            return false;
        }
        self.sample_list_scrollbar_track_bounds = Some(bounds);
        let grabbed_thumb = geometry.thumb.contains(&position);
        let grab_offset = if grabbed_thumb {
            position.y - geometry.thumb.origin.y
        } else {
            geometry.thumb.size.height / 2.0
        };
        self.sample_list_scrollbar_drag = Some(DragState { grab_offset });
        if !grabbed_thumb {
            self.update_sample_list_scrollbar_drag(position);
        }
        true
    }

    fn update_sample_list_scrollbar_drag(&mut self, position: Point<Pixels>) -> bool {
        let Some(drag) = self.sample_list_scrollbar_drag else {
            return false;
        };
        let Some(bounds) = self.sample_list_scrollbar_track_bounds else {
            return false;
        };
        let content_height = self
            .list_scroll_handle
            .0
            .borrow()
            .last_item_size
            .map_or(px(0.0), |size| size.contents.height);
        let current_offset = self.list_scroll_handle.0.borrow().base_handle.offset().y;
        let Some(geometry) = geometry(bounds, content_height, current_offset) else {
            return false;
        };
        let offset = offset_for_pointer(geometry, position.y, drag.grab_offset);
        if offset == current_offset {
            return false;
        }
        let handle = &self.list_scroll_handle.0.borrow().base_handle;
        let current = handle.offset();
        handle.set_offset(point(current.x, offset));
        true
    }

    fn finish_sample_list_scrollbar_drag(&mut self, position: Point<Pixels>) -> bool {
        let changed = self.update_sample_list_scrollbar_drag(position);
        let had_drag = self.sample_list_scrollbar_drag.take().is_some();
        self.sample_list_scrollbar_track_bounds = None;
        changed || had_drag
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn vertical_thumb_tracks_content_and_drag_position() {
        let track = Bounds {
            origin: point(px(10.0), px(20.0)),
            size: size(px(14.0), px(400.0)),
        };
        let geometry = geometry(track, px(1600.0), px(-400.0)).expect("scrollable list");
        assert_eq!(geometry.thumb.size.height, px(100.0));
        assert_eq!(geometry.thumb.origin.y, px(120.0));
        assert_eq!(
            offset_for_pointer(geometry, px(270.0), px(50.0)),
            px(-800.0)
        );
    }
}
