//! Accessible Harvest dropdown and independent enable control.
use super::{Browser, close_menu, mode, select, status_label, toggle, toggle_menu};
use crate::theme::color;
use gpui::{ClickEvent, Context, Window, div, prelude::*, px};
use yield_design::Theme;
use yield_session::HarvestFilterChoice;
/// Keep the two controls operable when macOS keyboard focus is on their AX handles.
pub(in crate::browser) fn handle_key_down(
    browser: &mut Browser,
    key: &str,
    is_held: bool,
    has_modifiers: bool,
    window: &mut Window,
    cx: &mut Context<Browser>,
) -> bool {
    if has_modifiers || browser.active_source.is_none() {
        return false;
    }
    let toggle_focused = browser
        .ax_focus(super::super::accessibility::HARVEST_FILTER_TOGGLE)
        .is_some_and(|focus| focus.is_focused(window));
    let scope_focus = browser.ax_focus(super::super::accessibility::HARVEST_FILTER_SCOPE);
    let scope_focused = scope_focus
        .as_ref()
        .is_some_and(|focus| focus.is_focused(window));
    let choices = HarvestFilterChoice::ALL;
    let choice_focused = browser
        .harvest_filter
        .menu_open
        .then(|| {
            choices.iter().enumerate().find_map(|(index, _)| {
                browser
                    .ax_focus(&super::super::accessibility::harvest_choice_id(index))
                    .filter(|focus| focus.is_focused(window))
                    .map(|_| index)
            })
        })
        .flatten();
    if key == "escape" && browser.harvest_filter.menu_open {
        close_menu(browser, cx);
        if let Some(focus) = scope_focus {
            window.focus(&focus);
        }
        cx.notify();
        return true;
    }
    if is_held {
        return toggle_focused || scope_focused || choice_focused.is_some();
    }
    if matches!(key, "enter" | "space") {
        if toggle_focused {
            toggle(browser, cx);
            return true;
        }
        if let Some(index) = choice_focused {
            select(browser, choices[index], cx);
            if let Some(focus) = scope_focus {
                window.focus(&focus);
            }
            return true;
        }
        if scope_focused {
            toggle_menu(browser, cx);
            return true;
        }
    }
    if matches!(key, "up" | "arrowup" | "down" | "arrowdown")
        && (scope_focused || choice_focused.is_some())
    {
        if !browser.harvest_filter.menu_open {
            browser.harvest_filter.menu_open = true;
        }
        let down = matches!(key, "down" | "arrowdown");
        let index = choice_focused
            .map(|index| {
                if down {
                    (index + 1) % choices.len()
                } else {
                    (index + choices.len() - 1) % choices.len()
                }
            })
            .unwrap_or(if down { 0 } else { choices.len() - 1 });
        if let Some(focus) =
            browser.ax_focus(&super::super::accessibility::harvest_choice_id(index))
        {
            window.focus(&focus);
        }
        cx.notify();
        return true;
    }
    // Focused queue controls must not fall through to sample audition or curation shortcuts.
    (toggle_focused || scope_focused || choice_focused.is_some()) && key != "tab"
}

/// Render source-scoped Harvest enable and scope controls.
pub(in crate::browser) fn render(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> gpui::Stateful<gpui::Div> {
    let mode = mode(browser);
    let active_source = browser.active_source.is_some();
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let warning = color(theme.colors.warning);
    div()
        .id("harvest-filter-controls")
        .when(browser.harvest_filter.menu_open, |element| {
            element.on_mouse_down_out(cx.listener(|view, _: &gpui::MouseDownEvent, _, cx| {
                close_menu(view, cx);
            }))
        })
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .pb(px(theme.spacing.control_gap))
        .border_t(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .child(
            div()
                .flex()
                .items_center()
                .h(px(theme.spacing.row_height))
                .px(px(theme.spacing.panel_padding))
                .gap(px(theme.spacing.control_gap))
                .text_size(px(theme.typography.small_size))
                .child(
                    div()
                        .id("harvest-filter-toggle")
                        .debug_selector(|| "harvest-filter-toggle".into())
                        .when_some(
                            browser.ax_focus(super::super::accessibility::HARVEST_FILTER_TOGGLE),
                            |element, focus| element.track_focus(&focus),
                        )
                        .h(px(theme.spacing.row_height))
                        .items_center()
                        .cursor_pointer()
                        .text_color(color(if mode.enabled {
                            theme.colors.accent
                        } else {
                            theme.colors.text_muted
                        }))
                        .hover(move |style| style.bg(hover))
                        .active(move |style| style.bg(pressed))
                        .when(!active_source, |element| element.opacity(0.45))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            if active_source {
                                toggle(view, cx);
                            }
                        }))
                        .child("HARVEST")
                        .when_some(
                            browser.ax_marker(super::super::accessibility::HARVEST_FILTER_TOGGLE),
                            |element, marker| element.child(marker),
                        ),
                )
                .child(
                    div()
                        .id("harvest-filter-scope")
                        .debug_selector(|| "harvest-filter-scope".into())
                        .when_some(
                            browser.ax_focus(super::super::accessibility::HARVEST_FILTER_SCOPE),
                            |element, focus| element.track_focus(&focus),
                        )
                        .flex_1()
                        .min_w_0()
                        .h(px(theme.spacing.row_height))
                        .items_center()
                        .cursor_pointer()
                        .text_color(color(theme.colors.text_primary))
                        .hover(move |style| style.bg(hover))
                        .active(move |style| style.bg(pressed))
                        .when(!active_source, |element| element.opacity(0.45))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            if active_source {
                                toggle_menu(view, cx);
                            }
                        }))
                        .child(format!("{} ▾", mode.choice.label()))
                        .when_some(
                            browser.ax_marker(super::super::accessibility::HARVEST_FILTER_SCOPE),
                            |element, marker| element.child(marker),
                        ),
                ),
        )
        .when(browser.harvest_filter.menu_open, |element| {
            element.child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(theme.spacing.panel_padding))
                    .children(HarvestFilterChoice::ALL.into_iter().enumerate().map(
                        |(index, scope)| {
                            div()
                                .id(("harvest-filter-choice", index))
                                .debug_selector(move || format!("harvest-filter-choice-{index}"))
                                .when_some(
                                    browser.ax_focus(
                                        &super::super::accessibility::harvest_choice_id(index),
                                    ),
                                    |element, focus| element.track_focus(&focus),
                                )
                                .h(px(theme.spacing.row_height))
                                .items_center()
                                .cursor_pointer()
                                .text_size(px(theme.typography.small_size))
                                .text_color(color(if mode.choice == scope {
                                    theme.colors.accent
                                } else {
                                    theme.colors.text_primary
                                }))
                                .hover(move |style| style.bg(hover))
                                .active(move |style| style.bg(pressed))
                                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                    select(view, scope, cx);
                                    if let Some(focus) = view
                                        .ax_focus(super::super::accessibility::HARVEST_FILTER_SCOPE)
                                    {
                                        window.focus(&focus);
                                    }
                                }))
                                .child(scope.label())
                                .when_some(
                                    browser.ax_marker(
                                        &super::super::accessibility::harvest_choice_id(index),
                                    ),
                                    |element, marker| element.child(marker),
                                )
                        },
                    )),
            )
        })
        .children(status_label(browser).into_iter().map(|message| {
            div()
                .px(px(theme.spacing.panel_padding))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(theme.typography.small_size))
                .text_color(warning)
                .child(message)
        }))
}
