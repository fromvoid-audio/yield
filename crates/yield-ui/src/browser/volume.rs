#![deny(missing_docs)]
#![deny(warnings)]

//! Compact audition-volume control for the browser toolbar.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    Bounds, Context, DragMoveEvent, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    Render, Window, canvas, div, prelude::*, px,
};
use yield_session::PreferencePatch;

use crate::theme::color;

use super::Browser;

/// The built-in audition volume used before preferences have loaded.
pub(super) const DEFAULT_VOLUME: f32 = 1.0;
const SLIDER_WIDTH: f32 = 112.0;
const SLIDER_HEIGHT: f32 = 16.0;
const TRACK_HEIGHT: f32 = 8.0;
const KEYBOARD_STEP: f32 = 0.05;
const QUANTIZATION_STEPS: f32 = 1_000.0;
const MAX_RUNTIME_RETRIES: u8 = 3;

/// State retained while the pointer owns a volume-slider drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct VolumeDrag {
    bounds: Bounds<Pixels>,
    initial: f32,
}

#[derive(Clone, Copy, Debug)]
struct VolumeDragToken;

struct VolumeDragGhost;

impl Render for VolumeDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Clamp invalid or out-of-range volume input to a safe linear value.
pub(super) fn sanitize_volume(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        DEFAULT_VOLUME
    }
}

/// Quantize volume changes before sending them to the session service and runtime.
pub(super) fn quantize_volume(value: f32) -> f32 {
    (sanitize_volume(value) * QUANTIZATION_STEPS).round() / QUANTIZATION_STEPS
}

fn volume_for_position(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<f32> {
    let width = f32::from(bounds.size.width);
    let left = f32::from(bounds.origin.x);
    let x = f32::from(position.x);
    if !width.is_finite() || width <= 0.0 || !left.is_finite() || !x.is_finite() {
        return None;
    }
    Some(quantize_volume(((x - left) / width).clamp(0.0, 1.0)))
}

fn keyboard_volume(value: f32, key: &str, precise: bool) -> Option<f32> {
    let value = sanitize_volume(value);
    let step = if precise { 0.01 } else { KEYBOARD_STEP };
    let candidate = match key {
        "arrowleft" | "left" | "arrowdown" | "down" => value - step,
        "arrowright" | "right" | "arrowup" | "up" => value + step,
        "home" => 0.0,
        "end" => 1.0,
        _ => return None,
    };
    Some(quantize_volume(candidate))
}

/// Return whether a key is handled by the focused volume control.
pub(super) fn is_keyboard_key(key: &str) -> bool {
    keyboard_volume(0.5, key, false).is_some()
}

/// Render the reference-sized volume slider and install pointer capture handlers.
pub(super) fn render_slider(
    browser: &Browser,
    theme: yield_design::Theme,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let bounds = Rc::new(RefCell::new(None));
    let editable = browser.preferences_editable();
    let value = quantize_volume(browser.volume);
    let track_color = if editable {
        theme.colors.control_disabled_fill
    } else {
        theme.colors.border
    };
    let fill_color = if editable {
        theme.colors.text_primary
    } else {
        theme.colors.text_disabled
    };
    let focus_color = color(theme.interaction.focus_outline);
    let focus_width = theme.interaction.focus_outline_width;
    let slider_bounds = Rc::clone(&bounds);
    let mut slider = div()
        .id("volume-slider")
        .relative()
        .flex_none()
        .w(px(SLIDER_WIDTH))
        .h(px(SLIDER_HEIGHT))
        .track_focus(&browser.volume_focus_handle)
        .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
        .child(
            div()
                .absolute()
                .left_0()
                .top(px((SLIDER_HEIGHT - TRACK_HEIGHT) / 2.0))
                .w(px(SLIDER_WIDTH))
                .h(px(TRACK_HEIGHT))
                .bg(color(track_color)),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top(px((SLIDER_HEIGHT - TRACK_HEIGHT) / 2.0))
                .w(px(SLIDER_WIDTH * value))
                .h(px(TRACK_HEIGHT))
                .bg(color(fill_color)),
        )
        .child(
            canvas(
                move |layout_bounds, _, _| {
                    *slider_bounds.borrow_mut() = Some(layout_bounds);
                },
                move |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0(),
        );

    if editable {
        let bounds_for_down = Rc::clone(&bounds);
        slider = slider
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    window.focus(&view.volume_focus_handle);
                    let Some(bounds) = bounds_for_down.borrow().as_ref().copied() else {
                        return;
                    };
                    view.begin_volume_drag(event.position, bounds, cx);
                }),
            )
            .on_drag(VolumeDragToken, |_, _, _, cx| cx.new(|_| VolumeDragGhost))
            .on_drag_move::<VolumeDragToken>(cx.listener(
                |view, event: &DragMoveEvent<VolumeDragToken>, _, cx| {
                    view.update_volume_drag(event.event.position, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    view.finish_volume_drag(event.position, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    view.finish_volume_drag(event.position, cx);
                }),
            );
    }

    div()
        .id("volume-control")
        .flex()
        .flex_none()
        .items_center()
        .gap(px(theme.spacing.control_gap))
        .h(px(theme.spacing.control_height))
        .text_size(px(theme.typography.small_size))
        .text_color(if editable {
            color(theme.colors.text_muted)
        } else {
            color(theme.colors.text_disabled)
        })
        .child("VOL")
        .child(slider)
}

impl Browser {
    pub(super) fn change_volume_from_key(
        &mut self,
        key: &str,
        precise: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return true;
        }
        let Some(value) = keyboard_volume(self.volume, key, precise) else {
            return false;
        };
        self.set_volume(value, cx);
        true
    }

    fn begin_volume_drag(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        self.volume_drag = Some(VolumeDrag {
            bounds,
            initial: self.volume,
        });
        self.update_volume_drag(position, cx);
    }

    fn update_volume_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.volume_drag else {
            return;
        };
        let Some(value) = volume_for_position(position, drag.bounds) else {
            return;
        };
        self.set_volume(value, cx);
    }

    fn finish_volume_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.volume_drag.take() else {
            return;
        };
        if let Some(value) = volume_for_position(position, drag.bounds) {
            self.set_volume(value, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_volume_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.volume_drag.take() else {
            return false;
        };
        cx.stop_active_drag(window);
        if (self.volume - drag.initial).abs() > f32::EPSILON {
            self.set_volume(drag.initial, cx);
        }
        true
    }

    fn set_volume(&mut self, value: f32, cx: &mut Context<Self>) {
        let value = quantize_volume(value);
        let changed = self.volume.to_bits() != value.to_bits();
        if changed {
            if !self.submit_preference_patch(PreferencePatch::SetOutputVolume(value), cx) {
                cx.notify();
                return;
            }
            self.volume = value;
            if let Some(intent) = self.playback_init_intent.as_mut() {
                intent.set_volume(value);
            }
            if let Some(intent) = self.audio_output_resume.as_mut() {
                intent.set_volume(value);
            }
            self.volume_runtime_pending = Some(value);
            self.volume_runtime_retry_count = 0;
            self.sync_runtime_volume();
        } else if self.volume_runtime_pending == Some(value) {
            self.sync_runtime_volume();
        }
        cx.notify();
    }

    pub(super) fn apply_preference_volume(&mut self, value: f32) {
        let value = quantize_volume(value);
        let changed = self.volume.to_bits() != value.to_bits();
        self.volume = value;
        if let Some(intent) = self.playback_init_intent.as_mut() {
            intent.set_volume(value);
        }
        if let Some(intent) = self.audio_output_resume.as_mut() {
            intent.set_volume(value);
        }
        if changed {
            self.volume_runtime_pending = Some(value);
            self.volume_runtime_retry_count = 0;
            self.sync_runtime_volume();
        } else if self.volume_runtime_pending == Some(value) {
            self.sync_runtime_volume();
        }
    }

    pub(super) fn retry_runtime_volume(&mut self) {
        // Called once for each newly-created playback runtime, so a fresh handle gets
        // its own bounded retry budget without creating an idle retry loop.
        self.volume_runtime_retry_count = 0;
        self.volume_runtime_pending = Some(self.volume);
        self.sync_runtime_volume();
    }

    pub(super) fn mark_volume_applied_by_play(&mut self) {
        self.volume_runtime_pending = None;
        self.volume_runtime_retry_count = 0;
        self.volume_status = None;
    }

    fn sync_runtime_volume(&mut self) {
        let Some(desired) = self.volume_runtime_pending else {
            return;
        };
        if self.playback.is_none() || self.volume_runtime_retry_count >= MAX_RUNTIME_RETRIES {
            return;
        }
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            return;
        };
        match handle.try_set_volume(desired) {
            Ok(()) => {
                self.volume_runtime_pending = None;
                self.volume_runtime_retry_count = 0;
                self.volume_status = None;
            }
            Err(error) => {
                self.volume_runtime_retry_count = self.volume_runtime_retry_count.saturating_add(1);
                self.volume_status =
                    Some(if self.volume_runtime_retry_count >= MAX_RUNTIME_RETRIES {
                        format!("Volume update failed; change volume to retry ({error:?})")
                    } else {
                        format!("Could not update audition volume: {error:?}")
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn bounds(width: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(10.0), px(0.0)), gpui::size(px(width), px(16.0)))
    }

    #[test]
    fn volume_sanitizes_non_finite_and_clamps() {
        assert_eq!(sanitize_volume(f32::NAN), DEFAULT_VOLUME);
        assert_eq!(sanitize_volume(f32::INFINITY), DEFAULT_VOLUME);
        assert_eq!(sanitize_volume(f32::NEG_INFINITY), DEFAULT_VOLUME);
        assert_eq!(sanitize_volume(-0.4), 0.0);
        assert_eq!(sanitize_volume(1.4), 1.0);
    }

    #[test]
    fn pointer_volume_clamps_and_quantizes_at_bounds() {
        let bounds = bounds(100.0);
        assert_eq!(
            volume_for_position(point(px(-10.0), px(8.0)), bounds),
            Some(0.0)
        );
        assert_eq!(
            volume_for_position(point(px(60.0), px(8.0)), bounds),
            Some(0.5)
        );
        assert_eq!(
            volume_for_position(point(px(140.0), px(8.0)), bounds),
            Some(1.0)
        );
    }

    #[test]
    fn keyboard_volume_matches_reference_step_and_endpoints() {
        assert_eq!(keyboard_volume(0.5, "arrowright", false), Some(0.55));
        assert_eq!(keyboard_volume(0.5, "arrowleft", false), Some(0.45));
        assert_eq!(keyboard_volume(0.5, "arrowright", true), Some(0.51));
        assert_eq!(keyboard_volume(0.5, "home", false), Some(0.0));
        assert_eq!(keyboard_volume(0.5, "end", false), Some(1.0));
        assert_eq!(keyboard_volume(0.5, "pageup", false), None);
    }

    #[test]
    fn quantization_bounds_runtime_and_persistence_updates() {
        assert_eq!(quantize_volume(0.1244), 0.124);
        assert_eq!(quantize_volume(0.1246), 0.125);
        assert_eq!(quantize_volume(2.0), 1.0);
    }
}
