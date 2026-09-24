//! Compact numeric input for the BPM toolbar's beat-guide count.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    MouseDownEvent, Render, ScrollDelta, ScrollWheelEvent, Subscription, Window, div, prelude::*,
    px,
};

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
}

/// Reusable compact numeric editor for a beat-guide count.
pub(crate) struct BeatCountInput {
    input: Entity<SearchInput>,
    focus_handle: FocusHandle,
    count: u8,
    _input_subscription: Subscription,
    blur_subscription: Option<Subscription>,
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
        }
    }

    /// Return the current valid beat count.
    pub(crate) fn count(&self) -> u8 {
        self.count
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
            cx.emit(BeatCountChanged { count });
            cx.notify();
        }
    }

    fn on_text_changed(&mut self, content: &str, cx: &mut Context<Self>) {
        let Some(count) = parse_in_range_beat_count(content) else {
            return;
        };
        if self.count == count {
            return;
        }
        self.count = count;
        cx.emit(BeatCountChanged { count });
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

    fn on_mouse_down(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.select_all_content(cx));
    }
}

impl Render for BeatCountInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.blur_subscription.is_none() {
            let focus_handle = self.input.read(cx).focus_handle();
            self.blur_subscription = Some(cx.on_blur(&focus_handle, window, |view, _, cx| {
                view.commit(cx);
            }));
        }
        let theme = active(cx);
        div()
            .flex_none()
            .w(px(34.0))
            .h(px(theme.spacing.control_height))
            .key_context("BeatCountInput")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .child(self.input.clone())
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
    use super::{
        DEFAULT_BEAT_COUNT, MAX_BEAT_COUNT, MIN_BEAT_COUNT, clamp_beat_count, committed_beat_count,
        parse_in_range_beat_count, step_beat_count,
    };

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
