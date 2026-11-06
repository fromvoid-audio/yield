//! Compact numeric input for the BPM toolbar's beat-guide count.

use gpui::{
    App, Context, CursorStyle, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, Subscription, Window, canvas, div, prelude::*, px,
};
use std::ops::RangeInclusive;

use crate::{
    search_input::{SearchChanged, SearchInput, SearchInputOptions},
    theme::active,
};

/// The default number of beats used by the BPM toolbar.
pub(crate) const DEFAULT_BEAT_COUNT: u8 = 4;
/// The smallest valid number of beats used by the BPM toolbar.
pub(crate) const MIN_BEAT_COUNT: u8 = 2;
/// The largest valid number of beats used by the BPM toolbar.
pub(crate) const MAX_BEAT_COUNT: u8 = 32;

/// A valid beat-count update from [`BeatCountInput`].
pub(crate) struct BeatCountChanged {
    /// The new beat count.
    pub(crate) count: u8,
    /// Whether the count can be applied to the live metronome.
    pub(crate) committed: bool,
}

/// Pointer displacement for an opt-in integer scrubber. The input supplies the value domain.
struct NumericScrub {
    press: Point<Pixels>,
    baseline: i32,
    pixels_per_step: f32,
    preview: Option<i32>,
}

enum ScrubMotion {
    Pending,
    SelectText,
    Preview(i32),
}

impl NumericScrub {
    fn motion(
        &mut self,
        position: Point<Pixels>,
        step: i32,
        limits: RangeInclusive<i32>,
    ) -> ScrubMotion {
        let delta = self.press - position;
        let vertical = f32::from(delta.y);
        let horizontal = f32::from(delta.x).abs();
        let threshold = self.pixels_per_step;
        if self.preview.is_none() {
            if horizontal >= threshold && horizontal >= vertical.abs() {
                return ScrubMotion::SelectText;
            }
            if vertical.abs() < threshold {
                return ScrubMotion::Pending;
            }
        }
        let steps = (vertical.abs() - threshold).max(0.0) / self.pixels_per_step;
        let offset = (steps.copysign(vertical) as i32).saturating_mul(step);
        let value = self
            .baseline
            .saturating_add(offset)
            .clamp(*limits.start(), *limits.end());
        self.preview = Some(value);
        ScrubMotion::Preview(value)
    }
}

/// Reusable compact numeric editor for a beat-guide count.
pub(crate) struct BeatCountInput {
    input: Entity<SearchInput>,
    focus_handle: FocusHandle,
    count: u8,
    _input_subscription: Subscription,
    blur_subscription: Option<Subscription>,
    activation_subscription: Option<Subscription>,
    scrub: Option<NumericScrub>,
}

impl EventEmitter<BeatCountChanged> for BeatCountInput {}

impl BeatCountInput {
    /// Create a beat-count input initialized to the toolbar default.
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            SearchInput::new_with_options_and_content(
                cx,
                SearchInputOptions::beat_count(),
                DEFAULT_BEAT_COUNT.to_string(),
            )
        });
        let input_subscription = cx.subscribe(&input, |view, input, _: &SearchChanged, cx| {
            let content = input.read(cx).content().to_owned();
            view.on_text_changed(&content, cx);
        });
        let focus_handle = input.read(cx).focus_handle();
        Self {
            input,
            focus_handle,
            count: DEFAULT_BEAT_COUNT,
            _input_subscription: input_subscription,
            blur_subscription: None,
            activation_subscription: None,
            scrub: None,
        }
    }

    /// Return the current valid beat count.
    pub(crate) fn count(&self) -> u8 {
        self.count
    }

    /// Return the visible draft, including an incomplete numeric edit.
    pub(crate) fn content(&self, cx: &App) -> String {
        self.input.read(cx).content().to_owned()
    }

    /// Route an accessibility edit through the same draft parser as typing.
    pub(crate) fn set_content(&mut self, content: String, cx: &mut Context<Self>) {
        self.finish_scrub(false, cx);
        self.input
            .update(cx, |input, cx| input.set_content(content, cx));
    }

    /// Return the input's focus handle for toolbar focus routing.
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Set and publish a valid beat count.
    pub(crate) fn set_count(&mut self, count: u8, cx: &mut Context<Self>) {
        let count = clamp_beat_count(u16::from(count));
        let changed = self.count != count;
        self.count = count;
        let content = count.to_string();
        if self.input.read(cx).content() != content {
            self.input
                .update(cx, |input, cx| input.set_content(content, cx));
        }
        if changed {
            cx.emit(BeatCountChanged {
                count,
                committed: true,
            });
            cx.notify();
        }
    }

    fn on_text_changed(&mut self, content: &str, cx: &mut Context<Self>) {
        if self
            .scrub
            .as_ref()
            .is_some_and(|scrub| scrub.preview.is_some())
        {
            return;
        }
        let Some(count) = parse_in_range_beat_count(content) else {
            return;
        };
        if self.count == count {
            return;
        }
        self.count = count;
        cx.emit(BeatCountChanged {
            count,
            committed: true,
        });
        cx.notify();
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let content = self.input.read(cx).content().to_owned();
        self.set_count(committed_beat_count(&content), cx);
    }

    fn step(&mut self, delta: i8, cx: &mut Context<Self>) {
        let content = self.input.read(cx).content().to_owned();
        let current = committed_beat_count(&content);
        self.set_count(step_beat_count(current, delta), cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" | "arrowup" => {
                self.step(1, cx);
                cx.stop_propagation();
            }
            "down" | "arrowdown" => {
                self.step(-1, cx);
                cx.stop_propagation();
            }
            "enter" if !event.is_held && !self.input.read(cx).is_composing() => {
                self.commit(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scrub.is_some() {
            return;
        }
        let (horizontal_delta, vertical_delta) = match event.delta {
            ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
            ScrollDelta::Lines(delta) => (delta.x, delta.y),
        };
        if vertical_delta.abs() < horizontal_delta.abs() {
            return;
        }
        if vertical_delta < 0.0 {
            self.step(1, cx);
            cx.stop_propagation();
        } else if vertical_delta > 0.0 {
            self.step(-1, cx);
            cx.stop_propagation();
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.finish_scrub(false, cx);
        self.input
            .update(cx, |input, cx| input.select_all_content(cx));
        if !event.modifiers.shift {
            // Scale with the control's logical size. GPUI already supplies logical coordinates.
            let pixels_per_step = (active(cx).spacing.control_height / 5.0).max(2.0);
            self.scrub = Some(NumericScrub {
                press: event.position,
                baseline: i32::from(self.count),
                pixels_per_step,
                preview: None,
            });
        }
    }

    fn update_scrub(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(scrub) = &mut self.scrub else {
            return false;
        };
        let previous = scrub.preview;
        match scrub.motion(
            position,
            1,
            i32::from(MIN_BEAT_COUNT)..=i32::from(MAX_BEAT_COUNT),
        ) {
            ScrubMotion::Pending => false,
            ScrubMotion::SelectText => {
                self.scrub = None;
                false
            }
            ScrubMotion::Preview(value) => {
                if previous != Some(value) {
                    self.input.update(cx, |input, cx| {
                        input.set_content(value.to_string(), cx);
                        if previous.is_none() {
                            input.set_pointer_cursor(CursorStyle::ResizeUpDown, cx);
                        }
                    });
                    if value != previous.unwrap_or(i32::from(self.count)) {
                        cx.emit(BeatCountChanged {
                            count: value as u8,
                            committed: false,
                        });
                    }
                }
                true
            }
        }
    }

    fn finish_scrub(&mut self, commit: bool, cx: &mut Context<Self>) -> bool {
        let Some(value) = self.scrub.take().and_then(|scrub| scrub.preview) else {
            return false;
        };
        self.input.update(cx, |input, cx| {
            input.set_pointer_cursor(CursorStyle::IBeam, cx)
        });
        if commit {
            self.set_count(value as u8, cx);
        } else {
            self.set_count(self.count, cx);
            if value != i32::from(self.count) {
                cx.emit(BeatCountChanged {
                    count: self.count,
                    committed: false,
                });
            }
        }
        true
    }

    fn scrub_capture(cx: &Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                // Continue receiving drag events outside the field and its hitbox.
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let Some(owner) = owner.upgrade() else {
                        return;
                    };
                    if owner.read(cx).scrub.is_some() {
                        let consumed = owner.update(cx, |view, cx| {
                            if event.pressed_button == Some(MouseButton::Left)
                                && view.focus_handle.is_focused(window)
                            {
                                view.update_scrub(event.position, cx)
                            } else {
                                view.finish_scrub(false, cx)
                            }
                        });
                        if consumed {
                            cx.stop_propagation();
                        }
                    }
                });
            },
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0))
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Focus can change before GPUI dispatches the next frame's blur notification.
        let focused = self.focus_handle.is_focused(window);
        if focused
            && self
                .scrub
                .as_ref()
                .is_some_and(|scrub| scrub.preview.is_some())
        {
            self.update_scrub(event.position, cx);
        }
        self.finish_scrub(focused, cx);
    }
}

impl Render for BeatCountInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.blur_subscription.is_none() {
            let focus_handle = self.input.read(cx).focus_handle();
            self.blur_subscription = Some(cx.on_blur(&focus_handle, window, |view, _, cx| {
                if !view.finish_scrub(false, cx) {
                    view.commit(cx);
                }
            }));
        }
        if self.activation_subscription.is_none() {
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |view, window, cx| {
                    if !window.is_window_active() {
                        view.finish_scrub(false, cx);
                    }
                }));
        }
        let theme = active(cx);
        div()
            .debug_selector(|| "beat-count-input".to_string())
            .flex_none()
            .w(px(34.0))
            .h(px(theme.spacing.control_height))
            .key_context("BeatCountInput")
            .capture_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| {
                if view.scrub.is_some() {
                    if event.keystroke.key == "escape" {
                        view.finish_scrub(false, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(self.input.clone())
            .child(Self::scrub_capture(cx))
    }
}

impl Focusable for BeatCountInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn parse_in_range_beat_count(value: &str) -> Option<u8> {
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let count = value.parse::<u16>().ok()?;
    (u16::from(MIN_BEAT_COUNT)..=u16::from(MAX_BEAT_COUNT))
        .contains(&count)
        .then_some(count as u8)
}

fn committed_beat_count(value: &str) -> u8 {
    value
        .is_ascii()
        .then(|| value.parse::<u16>().ok())
        .flatten()
        .map(clamp_beat_count)
        .unwrap_or(MIN_BEAT_COUNT)
}

fn clamp_beat_count(value: u16) -> u8 {
    value.clamp(u16::from(MIN_BEAT_COUNT), u16::from(MAX_BEAT_COUNT)) as u8
}

fn step_beat_count(current: u8, delta: i8) -> u8 {
    (i16::from(current) + i16::from(delta))
        .clamp(i16::from(MIN_BEAT_COUNT), i16::from(MAX_BEAT_COUNT)) as u8
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Modifiers, MouseButton, TestAppContext, point, px};

    use super::{
        BeatCountChanged, BeatCountInput, DEFAULT_BEAT_COUNT, MAX_BEAT_COUNT, MIN_BEAT_COUNT,
        NumericScrub, ScrubMotion, clamp_beat_count, committed_beat_count,
        parse_in_range_beat_count, step_beat_count,
    };

    #[test]
    fn scrub_steps_are_anchored_and_bounded() {
        let press = point(px(20.0), px(100.0));
        let new_scrub = || NumericScrub {
            press,
            baseline: 16,
            pixels_per_step: 4.0,
            preview: None,
        };
        let mut scrub = new_scrub();
        let mut move_up =
            |distance| scrub.motion(point(press.x, press.y - px(distance)), 1, 2..=32);
        assert!(matches!(move_up(3.0), ScrubMotion::Pending));
        assert!(matches!(move_up(4.0), ScrubMotion::Preview(16)));
        assert!(matches!(move_up(8.0), ScrubMotion::Preview(17)));
        assert!(matches!(move_up(16.0), ScrubMotion::Preview(19)));
        assert!(matches!(
            new_scrub().motion(point(press.x, press.y - px(16.0)), 1, 2..=32),
            ScrubMotion::Preview(19)
        ));
        assert!(matches!(move_up(100.0), ScrubMotion::Preview(32)));
        assert!(matches!(move_up(0.0), ScrubMotion::Preview(16)));
        assert!(matches!(move_up(-100.0), ScrubMotion::Preview(2)));
        assert!(matches!(
            new_scrub().motion(point(press.x + px(8.0), press.y), 1, 2..=32),
            ScrubMotion::SelectText
        ));
    }

    #[gpui::test]
    fn scrub_pointer_routes_preview_commit_and_escape(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (input, cx) = cx.add_window_view(|_, cx| BeatCountInput::new(cx));
        cx.refresh().expect("render beat-count input");
        let events = Rc::new(RefCell::new(Vec::new()));
        let captured_events = events.clone();
        let _subscription = cx.cx.update(|app| {
            app.subscribe(&input, move |_, event: &BeatCountChanged, _| {
                captured_events
                    .borrow_mut()
                    .push((event.count, event.committed));
            })
        });
        let bounds = cx
            .debug_bounds("beat-count-input")
            .expect("beat-count input");
        let start = point(bounds.left() + px(8.0), bounds.top() + px(8.0));
        let outside = point(bounds.right() + px(15.0), start.y + px(32.0));
        let plain = Modifiers::none();
        let left = MouseButton::Left;

        cx.simulate_mouse_down(start, left, plain);
        cx.simulate_mouse_move(point(start.x, start.y + px(1.0)), left, plain);
        cx.simulate_mouse_up(start, left, plain);
        assert!(events.borrow().is_empty());
        cx.simulate_keystrokes("up");
        assert_eq!(&*events.borrow(), &[(5, true)]);
        events.borrow_mut().clear();

        // Start vertically, then keep tracking after the cursor leaves the field.
        cx.simulate_mouse_down(start, left, plain);
        cx.simulate_mouse_move(point(start.x, outside.y), left, plain);
        cx.simulate_mouse_move(outside, left, plain);
        assert_eq!(&*events.borrow(), &[(2, false)]);
        assert_eq!(cx.cx.read(|app| input.read(app).count()), 5);
        assert_eq!(cx.cx.read(|app| input.read(app).content(app)), "2");
        cx.simulate_mouse_up(outside, left, plain);
        assert_eq!(&*events.borrow(), &[(2, false), (2, true)]);
        events.borrow_mut().clear();

        let higher = point(start.x, start.y - px(32.0));
        cx.simulate_mouse_down(start, left, plain);
        cx.simulate_mouse_move(higher, left, plain);
        assert!(
            events
                .borrow()
                .last()
                .is_some_and(|&(count, committed)| count > 2 && !committed)
        );
        cx.simulate_keystrokes("escape");
        cx.simulate_mouse_up(higher, left, plain);
        assert_eq!(events.borrow().last(), Some(&(2, false)));
        assert!(events.borrow().iter().all(|&(_, committed)| !committed));
        assert_eq!(cx.cx.read(|app| input.read(app).content(app)), "2");

        events.borrow_mut().clear();
        cx.simulate_mouse_down(start, left, plain);
        cx.simulate_mouse_move(higher, left, plain);
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.simulate_mouse_up(higher, left, plain);
        assert_eq!(events.borrow().last(), Some(&(2, false)));
        assert!(events.borrow().iter().all(|&(_, committed)| !committed));
        assert_eq!(cx.cx.read(|app| input.read(app).count()), 2);

        events.borrow_mut().clear();
        cx.simulate_click(start, plain);
        cx.simulate_input("12");
        assert_eq!(&*events.borrow(), &[(12, true)]);
    }

    #[test]
    fn production_count_rules_filter_to_ascii_digits() {
        assert_eq!(parse_in_range_beat_count("4"), Some(4));
        assert_eq!(parse_in_range_beat_count("02"), Some(2));
        assert_eq!(parse_in_range_beat_count("٤"), None);
        assert_eq!(parse_in_range_beat_count("a4"), None);
        assert_eq!(parse_in_range_beat_count(""), None);
    }

    #[test]
    fn commit_clamps_invalid_or_out_of_range_text() {
        assert_eq!(DEFAULT_BEAT_COUNT, 4);
        assert_eq!(committed_beat_count(""), MIN_BEAT_COUNT);
        assert_eq!(committed_beat_count("x"), MIN_BEAT_COUNT);
        assert_eq!(committed_beat_count("1"), MIN_BEAT_COUNT);
        assert_eq!(committed_beat_count("2"), MIN_BEAT_COUNT);
        assert_eq!(committed_beat_count("16"), 16);
        assert_eq!(committed_beat_count("99"), MAX_BEAT_COUNT);
        assert_eq!(committed_beat_count("٣"), MIN_BEAT_COUNT);
    }

    #[test]
    fn stepping_stays_within_the_native_count_range() {
        assert_eq!(step_beat_count(16, 1), 17);
        assert_eq!(step_beat_count(16, -1), 15);
        assert_eq!(step_beat_count(MIN_BEAT_COUNT, -1), MIN_BEAT_COUNT);
        assert_eq!(step_beat_count(MAX_BEAT_COUNT, 1), MAX_BEAT_COUNT);
        assert_eq!(
            step_beat_count(committed_beat_count(""), -1),
            MIN_BEAT_COUNT
        );
        assert_eq!(clamp_beat_count(0), MIN_BEAT_COUNT);
    }
}
