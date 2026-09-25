#![deny(missing_docs)]
#![deny(warnings)]

//! The bounded Browser shortcut-help modal.

use gpui::{
    AnyView, App, ClickEvent, Context, FocusHandle, MouseDownEvent, Pixels, Render, ScrollHandle,
    Size, Window, div, point, prelude::*, px,
};
use yield_design::Theme;

use crate::theme::color;

use super::Browser;

const MODAL_WIDTH: f32 = 620.0;
const MODAL_HEIGHT: f32 = 520.0;
const KEY_COLUMN_WIDTH: f32 = 194.0;
const BACKDROP_PADDING: f32 = 24.0;
const KEYBOARD_SCROLL_STEP: f32 = 44.0;

/// The focus and scroll state owned by the shortcut-help modal.
pub(super) struct ShortcutHelpState {
    open: bool,
    focus_handle: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll_handle: ScrollHandle,
}

impl ShortcutHelpState {
    /// Create a closed shortcut-help state with handles owned by the Browser.
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        Self {
            open: false,
            focus_handle: cx.focus_handle(),
            previous_focus: None,
            scroll_handle: ScrollHandle::new(),
        }
    }

    /// Return whether the modal currently owns Browser input.
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Borrow the modal's scroll handle for keyboard routing.
    pub(super) fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll_handle
    }
}

/// The action produced by Browser key routing while shortcut help is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyAction {
    /// Open the modal from the global Command-/ or Control-/ binding.
    Open,
    /// Close the modal and restore the focus that was active before opening.
    Close,
    /// Move the modal's bounded list by one line, one page, or to an edge.
    Scroll(ScrollAction),
    /// Consume the key while the modal is open.
    Consume,
    /// Leave the key for the existing Browser routing.
    Ignore,
}

/// A keyboard movement within the shortcut-help list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrollAction {
    /// Move the list up by one row.
    Up,
    /// Move the list down by one row.
    Down,
    /// Move the list up by one viewport.
    PageUp,
    /// Move the list down by one viewport.
    PageDown,
    /// Move the list to its first row.
    Home,
    /// Move the list to its last row.
    End,
}

/// Resolve global shortcut-help key routing before any Browser command handler.
pub(super) fn key_action(
    open: bool,
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> KeyAction {
    if open {
        if !held && key == "escape" {
            KeyAction::Close
        } else if !held && is_toggle_key(key, secondary, shift, alt, function) {
            KeyAction::Close
        } else if let Some(action) = scroll_action(key, secondary, shift, alt, function) {
            KeyAction::Scroll(action)
        } else {
            KeyAction::Consume
        }
    } else if !held && is_toggle_key(key, secondary, shift, alt, function) {
        KeyAction::Open
    } else {
        KeyAction::Ignore
    }
}

fn is_toggle_key(key: &str, secondary: bool, shift: bool, alt: bool, function: bool) -> bool {
    secondary && !shift && !alt && !function && key == "/"
}

fn scroll_action(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> Option<ScrollAction> {
    if secondary || shift || alt || function {
        return None;
    }
    match key {
        "up" | "arrowup" => Some(ScrollAction::Up),
        "down" | "arrowdown" => Some(ScrollAction::Down),
        "pageup" | "page_up" => Some(ScrollAction::PageUp),
        "pagedown" | "page_down" => Some(ScrollAction::PageDown),
        "home" => Some(ScrollAction::Home),
        "end" => Some(ScrollAction::End),
        _ => None,
    }
}

/// Apply one bounded keyboard movement to the modal's scroll handle.
pub(super) fn scroll(handle: &ScrollHandle, action: ScrollAction) {
    let offset = handle.offset();
    let max_offset = handle.max_offset();
    let current = f32::from(offset.y);
    let max = f32::from(max_offset.height).max(0.0);
    let page = f32::from(handle.bounds().size.height).max(KEYBOARD_SCROLL_STEP);
    let current_magnitude = (-current).clamp(0.0, max);
    let next_magnitude = match action {
        ScrollAction::Up => current_magnitude - KEYBOARD_SCROLL_STEP,
        ScrollAction::Down => current_magnitude + KEYBOARD_SCROLL_STEP,
        ScrollAction::PageUp => current_magnitude - page,
        ScrollAction::PageDown => current_magnitude + page,
        ScrollAction::Home => 0.0,
        ScrollAction::End => max,
    }
    .clamp(0.0, max);
    handle.set_offset(point(offset.x, px(-next_magnitude)));
}

impl Browser {
    /// Toggle the shortcut-help modal from its toolbar control.
    pub(super) fn toggle_shortcut_help_from_toolbar(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_shortcut_help(window, cx);
    }

    /// Toggle the shortcut-help modal while preserving the previous focus.
    pub(super) fn toggle_shortcut_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shortcut_help.open {
            self.close_shortcut_help(window, cx);
        } else {
            self.shortcut_help.previous_focus = window.focused(cx);
            self.cancel_file_drag_with_window(window, cx);
            self.cancel_waveform_gestures(window, cx);
            self.cancel_volume_drag(window, cx);
            self.shortcut_help.open = true;
            window.focus(&self.shortcut_help.focus_handle);
            cx.notify();
        }
    }

    fn close_shortcut_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shortcut_help.open {
            return;
        }
        self.shortcut_help.open = false;
        if let Some(previous_focus) = self.shortcut_help.previous_focus.take() {
            window.focus(&previous_focus);
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }
}

struct ShortcutHelpTooltip;

impl Render for ShortcutHelpTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_primary))
            .child("Show keyboard shortcuts (Cmd/Ctrl-/)")
    }
}

/// Build the toolbar tooltip for the shortcut-help control.
pub(super) fn shortcut_help_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| ShortcutHelpTooltip).into()
}

#[derive(Clone, Copy)]
struct ShortcutRow {
    keys: &'static str,
    description: &'static str,
}

struct ShortcutSection {
    title: &'static str,
    rows: &'static [ShortcutRow],
}

const GENERAL_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        keys: "Random button",
        description: "Play a random section of the selected sample",
    },
    ShortcutRow {
        keys: "Shift + Random",
        description: "Choose a listed sample, then a random section",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl + Random",
        description: "Toggle sticky random sections for Space",
    },
    ShortcutRow {
        keys: "Space",
        description: "Play/stop; random section in sticky mode",
    },
    ShortcutRow {
        keys: "Cmd-Space",
        description: "Play from the current waveform playhead",
    },
    ShortcutRow {
        keys: "Shift-R",
        description: "Play a random visible sample",
    },
    ShortcutRow {
        keys: "Cmd-Shift-R",
        description: "Play the previous random sample",
    },
    ShortcutRow {
        keys: "C",
        description: "Set the focused sample as compare anchor",
    },
    ShortcutRow {
        keys: "Shift-Space",
        description: "Replay the compare anchor without changing focus",
    },
    ShortcutRow {
        keys: "Esc",
        description: "Stop playback",
    },
    ShortcutRow {
        keys: "L",
        description: "Toggle loop playback",
    },
    ShortcutRow {
        keys: "X",
        description: "Toggle the focused sample in the selection",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl-A",
        description: "Select all visible samples",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl-C",
        description: "Copy selected files",
    },
    ShortcutRow {
        keys: "Cmd-Shift-L",
        description: "Copy status log",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl-F",
        description: "Focus sample search",
    },
    ShortcutRow {
        keys: "Up / Down",
        description: "Move sample focus",
    },
    ShortcutRow {
        keys: "Shift-Up / Shift-Down",
        description: "Extend the selection while moving",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl-Shift-Up / Down",
        description: "Retain the selection while moving",
    },
    ShortcutRow {
        keys: "Cmd/Ctrl-Left / Right",
        description: "Move through playback history",
    },
];

const CURATION_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        keys: "[ / ]",
        description: "Decrease or increase the selected rating",
    },
    ShortcutRow {
        keys: "1–6",
        description: "Toggle collection 1 through 6",
    },
];

const WAVEFORM_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        keys: "Left-drag",
        description: "Create a play selection; drag its handles to adjust",
    },
    ShortcutRow {
        keys: "Right-drag",
        description: "Create an edit selection; drag its handles to adjust",
    },
    ShortcutRow {
        keys: "Right-click waveform",
        description: "Clear the edit selection away from its handles",
    },
    ShortcutRow {
        keys: "Gain handle",
        description: "Drag vertically to preview edit gain",
    },
    ShortcutRow {
        keys: "Fade handles",
        description: "Drag horizontally to shape fades and their outer extensions",
    },
    ShortcutRow {
        keys: "Outer gain handle",
        description: "Drag vertically to set the gain outside the edit selection",
    },
    ShortcutRow {
        keys: "Double-click extension",
        description: "Clear that fade's outer extension",
    },
    ShortcutRow {
        keys: "Z",
        description: "Zoom the waveform to the play selection",
    },
    ShortcutRow {
        keys: "X",
        description: "Reset the waveform viewport",
    },
    ShortcutRow {
        keys: "Left / Right",
        description: "Slide the play selection",
    },
];

const INPUT_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        keys: "Up / Down",
        description: "Adjust the focused beat count",
    },
    ShortcutRow {
        keys: "Enter",
        description: "Commit the focused beat-guide count",
    },
    ShortcutRow {
        keys: "Enter",
        description: "Submit a tag or accept its selected completion",
    },
    ShortcutRow {
        keys: "Esc",
        description: "Leave a focused input or close its completion",
    },
    ShortcutRow {
        keys: "Up / Down",
        description: "Move through tag completions or categories",
    },
];

const MENU_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        keys: "Up / Down",
        description: "Move through context-menu actions",
    },
    ShortcutRow {
        keys: "Enter",
        description: "Activate the selected context-menu action",
    },
    ShortcutRow {
        keys: "Esc",
        description: "Close the context menu",
    },
];

const HELP_ROWS: &[ShortcutRow] = &[ShortcutRow {
    keys: "Cmd/Ctrl-/",
    description: "Open or close this shortcut list",
}];

const SECTIONS: &[ShortcutSection] = &[
    ShortcutSection {
        title: "BROWSER",
        rows: GENERAL_ROWS,
    },
    ShortcutSection {
        title: "CURATION",
        rows: CURATION_ROWS,
    },
    ShortcutSection {
        title: "WAVEFORM",
        rows: WAVEFORM_ROWS,
    },
    ShortcutSection {
        title: "INPUTS",
        rows: INPUT_ROWS,
    },
    ShortcutSection {
        title: "CONTEXT MENU",
        rows: MENU_ROWS,
    },
    ShortcutSection {
        title: "SHORTCUT HELP",
        rows: HELP_ROWS,
    },
];

fn render_row(row: ShortcutRow, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_start()
        .w_full()
        .min_h(px(theme.spacing.row_height))
        .gap(px(theme.spacing.control_gap))
        .child(
            div()
                .flex_none()
                .w(px(KEY_COLUMN_WIDTH))
                .text_color(color(theme.colors.accent))
                .child(row.keys),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .text_color(color(theme.colors.text_primary))
                .child(row.description),
        )
}

fn render_section(section: &ShortcutSection, theme: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(1.0))
        .child(
            div()
                .flex_none()
                .w_full()
                .pt(px(theme.spacing.panel_padding))
                .pb(px(theme.spacing.control_gap))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(section.title),
        )
        .children(
            section
                .rows
                .iter()
                .copied()
                .map(|row| render_row(row, theme)),
        )
}

/// Render the focus-owned, scrollable shortcut-help modal.
pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    viewport: Size<Pixels>,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let viewport_width = f32::from(viewport.width).max(0.0);
    let viewport_height = f32::from(viewport.height).max(0.0);
    let backdrop_padding = BACKDROP_PADDING
        .min(viewport_width / 2.0)
        .min(viewport_height / 2.0);
    let available_width = (viewport_width - backdrop_padding * 2.0).max(1.0);
    let available_height = (viewport_height - backdrop_padding * 2.0).max(1.0);
    let panel = div()
        .id("shortcut-help-panel")
        .flex()
        .flex_col()
        .w(px(MODAL_WIDTH))
        .h(px(MODAL_HEIGHT))
        .max_w(px(available_width))
        .max_h(px(available_height))
        .bg(color(theme.colors.surface_raised))
        .border(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border_emphasis))
        .rounded(px(4.0))
        .occlude()
        .track_focus(&browser.shortcut_help.focus_handle)
        .on_any_mouse_down(cx.listener(|_: &mut Browser, _: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
        }))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .w_full()
                .h(px(
                    theme.spacing.row_height + theme.spacing.panel_padding * 2.0
                ))
                .px(px(theme.spacing.panel_padding))
                .bg(color(theme.colors.surface_overlay))
                .border_b(px(theme.spacing.border_width))
                .border_color(color(theme.colors.border))
                .text_size(px(theme.typography.title_size))
                .text_color(color(theme.colors.text_primary))
                .child("SHORTCUTS")
                .child(div().flex_1())
                .child(
                    div()
                        .id("shortcut-help-close")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .w(px(theme.spacing.control_height))
                        .h(px(theme.spacing.control_height))
                        .cursor_pointer()
                        .text_size(px(theme.typography.title_size))
                        .text_color(color(theme.colors.text_muted))
                        .hover(|style| style.bg(color(theme.colors.selection_hover_fill)))
                        .active(|style| style.bg(color(theme.interaction.pressed_fill)))
                        .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                            view.close_shortcut_help(window, cx);
                        }))
                        .child("×"),
                ),
        )
        .child(
            div()
                .id("shortcut-help-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .px(px(theme.spacing.panel_padding))
                .pb(px(theme.spacing.panel_padding))
                .overflow_y_scroll()
                .track_scroll(&browser.shortcut_help.scroll_handle)
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .pt(px(theme.spacing.panel_padding))
                        .pb(px(theme.spacing.control_gap))
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(theme.colors.text_muted))
                        .child("Shortcuts depend on the focused area."),
                )
                .children(
                    SECTIONS
                        .iter()
                        .map(|section| render_section(section, &theme)),
                ),
        );

    let dismiss = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(backdrop_padding))
        .bg(color(theme.colors.background).opacity(0.72))
        .occlude()
        .on_any_mouse_down(cx.listener(|view, _: &MouseDownEvent, window, cx| {
            view.close_shortcut_help(window, cx);
            cx.stop_propagation();
        }))
        .child(panel);
    dismiss
}

#[cfg(test)]
mod tests {
    use super::{KeyAction, ScrollAction, key_action};

    #[test]
    fn command_slash_opens_only_on_an_unheld_secondary_key() {
        assert_eq!(
            key_action(false, "/", true, false, false, false, false),
            KeyAction::Open
        );
        assert_eq!(
            key_action(false, "/", false, false, false, false, false),
            KeyAction::Ignore
        );
        assert_eq!(
            key_action(false, "/", true, false, false, false, true),
            KeyAction::Ignore
        );
        assert_eq!(
            key_action(false, "escape", true, false, false, false, false),
            KeyAction::Ignore
        );
        for modifiers in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            assert_eq!(
                key_action(
                    false,
                    "/",
                    true,
                    modifiers.0,
                    modifiers.1,
                    modifiers.2,
                    false,
                ),
                KeyAction::Ignore
            );
        }
    }

    #[test]
    fn open_modal_consumes_background_keys_and_closes_on_escape_or_toggle() {
        assert_eq!(
            key_action(true, "space", false, false, false, false, false),
            KeyAction::Consume
        );
        assert_eq!(
            key_action(true, "a", true, false, false, false, false),
            KeyAction::Consume
        );
        assert_eq!(
            key_action(true, "space", false, false, false, false, true),
            KeyAction::Consume
        );
        assert_eq!(
            key_action(true, "escape", false, false, false, false, false),
            KeyAction::Close
        );
        assert_eq!(
            key_action(true, "/", true, false, false, false, false),
            KeyAction::Close
        );
        assert_eq!(
            key_action(true, "/", true, true, false, false, false),
            KeyAction::Consume
        );
        assert_eq!(
            key_action(true, "escape", false, false, false, false, true),
            KeyAction::Consume
        );
    }

    #[test]
    fn open_modal_routes_unmodified_navigation_to_its_bounded_scroll() {
        assert_eq!(
            key_action(true, "up", false, false, false, false, false),
            KeyAction::Scroll(ScrollAction::Up)
        );
        assert_eq!(
            key_action(true, "pagedown", false, false, false, false, false),
            KeyAction::Scroll(ScrollAction::PageDown)
        );
        assert_eq!(
            key_action(true, "home", false, false, false, false, false),
            KeyAction::Scroll(ScrollAction::Home)
        );
        assert_eq!(
            key_action(true, "end", false, false, false, false, true),
            KeyAction::Scroll(ScrollAction::End)
        );
        assert_eq!(
            key_action(true, "down", false, true, false, false, false),
            KeyAction::Consume
        );
    }
}
