#![deny(missing_docs)]
#![deny(warnings)]

//! Sources and folder-tree sidebar for the GPUI browser.

use std::path::PathBuf;

use gpui::{ClickEvent, Context, MouseButton, MouseDownEvent, div, prelude::*, px, uniform_list};
use yield_design::Theme;

use crate::theme::{color, transparent};

use super::Browser;
use super::curation;
use super::motion::{self, MotionFrame};
use super::tag_chips;
use super::tag_completion;
use super::tag_filter;
use super::tag_library;
use super::tags;

fn hover_rail(group: String, theme: &Theme) -> impl gpui::IntoElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(motion::HOVER_RAIL_WIDTH))
        .bg(motion::with_alpha(
            theme.interaction.focus_outline,
            motion::HOVER_RAIL_ALPHA,
        ))
        .opacity(0.0)
        .group_hover(group, |style| style.opacity(1.0))
}

fn pending_source_row(
    theme: &Theme,
    root: PathBuf,
    label: String,
    generation: u64,
    motion_frame: MotionFrame,
) -> impl gpui::IntoElement {
    let row_group = format!(
        "source-row:pending:{}:{}",
        generation,
        root.to_string_lossy()
    );
    div()
        .group(row_group.clone())
        .flex()
        .flex_none()
        .w_full()
        .items_center()
        .h(px(theme.spacing.row_height))
        .relative()
        .bg(transparent())
        .child(
            div()
                .flex_none()
                .w(px(theme.spacing.selected_marker_width))
                .h_full()
                .bg(transparent()),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .pl(px(theme.spacing.row_padding_x))
                .text_size(px(theme.typography.body_size))
                .text_color(color(theme.colors.warning))
                .child(label),
        )
        .child(hover_rail(row_group, theme))
        .child(motion::source_sweep_overlay(
            motion_frame.source_phase,
            motion::with_alpha(theme.colors.accent, motion::SOURCE_SWEEP_TRACK_ALPHA),
            motion::with_alpha(theme.colors.accent, motion::SOURCE_SWEEP_SEGMENT_ALPHA),
        ))
}

pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    motion_frame: MotionFrame,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    if tag_library::is_open(browser) {
        return tag_library::render(browser, theme, cx).into_any_element();
    }
    let source_rows = browser.source_rows();
    let pending_source = browser.pending_source_row();
    let source_count = source_rows.len() + usize::from(pending_source.is_some());
    let source_height = theme.spacing.row_height * source_count.min(5) as f32;
    let source_list = if source_rows.is_empty() {
        if let Some((root, label, generation)) = pending_source.clone() {
            div()
                .flex_none()
                .h(px(theme.spacing.row_height))
                .child(pending_source_row(
                    &theme,
                    root,
                    label,
                    generation,
                    motion_frame,
                ))
        } else {
            div()
                .flex()
                .flex_none()
                .h(px(52.0))
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.body_size))
                .text_color(color(theme.colors.text_muted))
                .child("No sources")
        }
    } else {
        let pending_slots = usize::from(pending_source.is_some());
        let visible_source_rows = source_count.min(5).saturating_sub(pending_slots).max(1);
        let source_list = div()
            .flex()
            .flex_col()
            .flex_none()
            .h(px(source_height.max(theme.spacing.row_height)))
            .child(
                div()
                    .relative()
                    .overflow_hidden()
                    .h(px(theme.spacing.row_height * visible_source_rows as f32))
                    .child(
                        uniform_list(
                            "source-list",
                            source_rows.len(),
                            cx.processor({
                                let list_theme = theme.clone();
                                move |view, range: std::ops::Range<usize>, window, cx| {
                                    #[cfg(target_os = "macos")]
                                    view.update_ax_source_range(range.clone(), window, cx);
                                    #[cfg(not(target_os = "macos"))]
                                    let _ = window;
                                    let source_rows = view.source_rows();
                                    range
                                        .filter_map(|visible_index| {
                                            let (index, root, label, selected, error) =
                                                source_rows.get(visible_index).cloned()?;
                                            let hover_fill =
                                                color(list_theme.colors.selection_hover_fill);
                                            let pressed_fill =
                                                color(list_theme.interaction.pressed_fill);
                                            let row_group =
                                                format!("source-row:{}", root.to_string_lossy());
                                            let (source_ax_id, forget_ax_id) = view.ax_source_ids(&root);
                                            let scanning = view.source_scan_is_pending(&root);
                                            let focused = motion_frame.focus_alpha > 0
                                                && view.focus_matches_source(&root);
                                            let source_row = div()
                                                .group(row_group.clone())
                                                .id(("source-row", index))
                                                .when_some(source_ax_id.as_deref().and_then(|id| view.ax_focus(id)), |element, focus| element.track_focus(&focus))
                                                .flex()
                                                .flex_none()
                                                .w_full()
                                                .items_center()
                                                .h(px(list_theme.spacing.row_height))
                                                .relative()
                                                .cursor_pointer()
                                                .bg(if selected {
                                                    color(list_theme.colors.selection_fill)
                                                } else {
                                                    transparent()
                                                })
                                                .hover(move |style| style.bg(hover_fill))
                                                .active(move |style| style.bg(pressed_fill))
                                                .on_click(cx.listener(
                                                    move |view, _: &ClickEvent, window, cx| {
                                                        view.select_source(index, window, cx);
                                                    },
                                                ))
                                                .on_mouse_down(
                                                    MouseButton::Right,
                                                    cx.listener(
                                                        move |view,
                                                              event: &MouseDownEvent,
                                                              window,
                                                              cx| {
                                                            view.open_source_context_menu(
                                                                index, event, window, cx,
                                                            );
                                                        },
                                                    ),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .w(px(
                                                            list_theme.spacing.selected_marker_width,
                                                        ))
                                                        .h_full()
                                                        .bg(if selected {
                                                            color(list_theme.colors.accent)
                                                        } else {
                                                            transparent()
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .text_ellipsis()
                                                        .pl(px(list_theme.spacing.row_padding_x))
                                                        .text_size(px(list_theme.typography.body_size))
                                                        .text_color(if selected {
                                                            color(list_theme.colors.text_primary)
                                                        } else {
                                                            color(list_theme.colors.text_muted)
                                                        })
                                                        .child(label),
                                                )
                                                .child(
                                                    div()
                                                        .id(("forget-source", index))
                                                        .relative()
                                                        .when_some(forget_ax_id.as_deref().and_then(|id| view.ax_focus(id)), |element, focus| element.track_focus(&focus))
                                                        .flex_none()
                                                        .w(px(24.0))
                                                        .items_center()
                                                        .justify_center()
                                                        .text_color(color(if error.is_some() {
                                                            list_theme.colors.danger
                                                        } else {
                                                            list_theme.colors.text_muted
                                                        }))
                                                        .on_click(cx.listener(
                                                            move |view, _: &ClickEvent, window, cx| {
                                                                cx.stop_propagation();
                                                                view.forget_source(index, window, cx);
                                                            },
                                                        ))
                                                        .child("×")
                                                        .when_some(forget_ax_id.and_then(|id| view.ax_marker(id)), |element, marker| element.child(marker)),
                                                )
                                                .child(hover_rail(row_group, &list_theme))
                                                .when_some(source_ax_id.and_then(|id| view.ax_marker(id)), |element, marker| element.child(marker));
                                            let source_row = if focused {
                                                source_row.child(
                                                    div()
                                                        .absolute()
                                                        .top_0()
                                                        .bottom_0()
                                                        .left_0()
                                                        .w(px(motion::FOCUS_MARKER_WIDTH))
                                                        .bg(motion::with_alpha(
                                                            list_theme.interaction.focus_outline,
                                                            motion_frame.focus_alpha,
                                                        )),
                                                )
                                            } else {
                                                source_row
                                            };
                                            Some(if scanning {
                                                source_row.child(motion::source_sweep_overlay(
                                                    motion_frame.source_phase,
                                                    motion::with_alpha(
                                                        list_theme.colors.accent,
                                                        motion::SOURCE_SWEEP_TRACK_ALPHA,
                                                    ),
                                                    motion::with_alpha(
                                                        list_theme.colors.accent,
                                                        motion::SOURCE_SWEEP_SEGMENT_ALPHA,
                                                    ),
                                                ))
                                            } else {
                                                source_row
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                }
                            }),
                        )
                        .h_full()
                        .track_scroll(browser.source_scroll_handle.clone()),
                    )
                    .child(motion::scroll_edge_fades(
                        browser.source_scroll_handle.clone(),
                        browser.source_edge_transitions.clone(),
                        theme.motion,
                        browser.motion_clock_origin,
                        theme.colors.surface_raised,
                    )),
            );
        if let Some((root, label, generation)) = pending_source {
            source_list.child(pending_source_row(
                &theme,
                root,
                label,
                generation,
                motion_frame,
            ))
        } else {
            source_list
        }
    };

    let folder_rows = browser.folder_rows();
    let folder_list = if folder_rows.is_empty() {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_size(px(theme.typography.body_size))
            .text_color(color(theme.colors.text_muted))
            .child(if browser.pending_source_label.is_some() {
                "Scanning source…"
            } else {
                "Choose a source folder"
            })
    } else {
        div()
            .flex_1()
            .h_full()
            .relative()
            .overflow_hidden()
            .child(
                uniform_list(
                    "folder-list",
                    folder_rows.len(),
                    cx.processor({
                        let list_theme = theme.clone();
                        move |view, range: std::ops::Range<usize>, window, cx| {
                            #[cfg(target_os = "macos")]
                            view.update_ax_folder_range(range.clone(), window, cx);
                            #[cfg(not(target_os = "macos"))]
                            let _ = window;
                            let folder_rows = view.folder_rows();
                            range
                                .filter_map(|visible_index| {
                                    let row = folder_rows.get(visible_index).cloned()?;
                                    let (folder_ax_id, toggle_ax_id) =
                                        view.ax_folder_ids(&row.path);
                                    let path_for_select = row.path.clone();
                                    let path_for_toggle = row.path.clone();
                                    let path_for_context = row.path.clone();
                                    let selected = view.folder_is_selected(&row.path);
                                    let focused = motion_frame.focus_alpha > 0
                                        && view.focus_matches_folder(&row.path);
                                    let hover_fill = color(list_theme.colors.selection_hover_fill);
                                    let pressed_fill = color(list_theme.interaction.pressed_fill);
                                    let indent = row.depth as f32 * 12.0;
                                    let row_group = format!("folder-row:{}", row.path.display());
                                    let toggle = if row.has_children
                                        && row.path != std::path::Path::new(".")
                                    {
                                        div()
                                            .id(("folder-toggle", visible_index))
                                            .relative()
                                            .when_some(
                                                toggle_ax_id
                                                    .as_deref()
                                                    .and_then(|id| view.ax_focus(id)),
                                                |element, focus| element.track_focus(&focus),
                                            )
                                            .flex_none()
                                            .w(px(18.0))
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .text_color(color(list_theme.colors.text_muted))
                                            .on_click(cx.listener(
                                                move |view, _: &ClickEvent, _, cx| {
                                                    cx.stop_propagation();
                                                    view.toggle_folder_expanded(
                                                        path_for_toggle.clone(),
                                                        cx,
                                                    );
                                                },
                                            ))
                                            .child(if row.expanded { "▾" } else { "▸" })
                                            .when_some(
                                                toggle_ax_id.and_then(|id| view.ax_marker(id)),
                                                |element, marker| element.child(marker),
                                            )
                                    } else {
                                        div()
                                            .id(("folder-toggle", visible_index))
                                            .flex_none()
                                            .w(px(18.0))
                                            .child("")
                                    };
                                    let label = if row.path == std::path::Path::new(".") {
                                        ".".to_string()
                                    } else {
                                        row.path
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| row.path.display().to_string())
                                    };
                                    let folder_row = div()
                                        .group(row_group.clone())
                                        .id(("folder-row", visible_index))
                                        .when_some(
                                            folder_ax_id
                                                .as_deref()
                                                .and_then(|id| view.ax_focus(id)),
                                            |element, focus| element.track_focus(&focus),
                                        )
                                        .flex()
                                        .flex_none()
                                        .w_full()
                                        .items_center()
                                        .h(px(list_theme.spacing.row_height))
                                        .relative()
                                        .cursor_pointer()
                                        .bg(if selected {
                                            color(list_theme.colors.selection_fill)
                                        } else {
                                            transparent()
                                        })
                                        .hover(move |style| style.bg(hover_fill))
                                        .active(move |style| style.bg(pressed_fill))
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, window, cx| {
                                                view.select_folder(
                                                    path_for_select.clone(),
                                                    window,
                                                    cx,
                                                );
                                            },
                                        ))
                                        .on_mouse_down(
                                            MouseButton::Right,
                                            cx.listener(
                                                move |view, event: &MouseDownEvent, window, cx| {
                                                    view.open_folder_context_menu(
                                                        path_for_context.clone(),
                                                        event,
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(px(list_theme.spacing.selected_marker_width))
                                                .h_full()
                                                .bg(if selected {
                                                    color(list_theme.colors.accent)
                                                } else {
                                                    transparent()
                                                }),
                                        )
                                        .child(div().flex_none().w(px(indent)))
                                        .child(toggle)
                                        .child(
                                            div()
                                                .flex_1()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .text_size(px(list_theme.typography.body_size))
                                                .text_color(if selected {
                                                    color(list_theme.colors.accent)
                                                } else if row.contains_audio {
                                                    color(list_theme.colors.text_primary)
                                                } else {
                                                    color(list_theme.colors.text_muted)
                                                })
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .pr(px(list_theme.spacing.row_padding_x))
                                                .text_size(px(list_theme.typography.small_size))
                                                .text_color(color(list_theme.colors.text_disabled))
                                                .child(row.audio_file_count.to_string()),
                                        )
                                        .child(hover_rail(row_group, &list_theme))
                                        .when_some(
                                            folder_ax_id.and_then(|id| view.ax_marker(id)),
                                            |element, marker| element.child(marker),
                                        );
                                    Some(if focused {
                                        folder_row.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .bottom_0()
                                                .left_0()
                                                .w(px(motion::FOCUS_MARKER_WIDTH))
                                                .bg(motion::with_alpha(
                                                    list_theme.interaction.focus_outline,
                                                    motion_frame.focus_alpha,
                                                )),
                                        )
                                    } else {
                                        folder_row
                                    })
                                })
                                .collect::<Vec<_>>()
                        }
                    }),
                )
                .h_full()
                .track_scroll(browser.folder_scroll_handle.clone()),
            )
            .child(motion::scroll_edge_fades(
                browser.folder_scroll_handle.clone(),
                browser.folder_edge_transitions.clone(),
                theme.motion,
                browser.motion_clock_origin,
                theme.colors.surface_raised,
            ))
    };

    let folder_list = folder_list
        .min_h(px(theme.spacing.row_height * 4.0))
        .debug_selector(|| String::from("sidebar-folder-list"));
    let scope_label = format!("Scope: {}", browser.folder_scope.label());
    let active_label = browser
        .active_source_label()
        .unwrap_or_else(|| "Sources".to_string());
    let active_error = browser
        .active_source_error()
        .map(|error| format!("Error: {error}"));
    let bottom_controls =
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .debug_selector(|| String::from("sidebar-bottom-controls"))
            .p(px(theme.spacing.panel_padding))
            .gap(px(theme.spacing.control_gap))
            .child(
                div()
                    .flex()
                    .w_full()
                    .gap(px(theme.spacing.control_gap))
                    .child(
                        div()
                            .id("folder-scope")
                            .relative()
                            .when_some(
                                browser.ax_focus("browser-folder-scope"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .debug_selector(|| String::from("folder-scope"))
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .justify_center()
                            .h(px(theme.spacing.control_height))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_size(px(theme.typography.small_size))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.toggle_folder_scope(cx)
                            }))
                            .child(scope_label)
                            .when_some(
                                browser.ax_marker("browser-folder-scope"),
                                |element, marker| element.child(marker),
                            ),
                    )
                    .child(
                        div()
                            .id("refresh-source")
                            .relative()
                            .when_some(
                                browser.ax_focus("browser-refresh-source"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .debug_selector(|| String::from("refresh-source"))
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .justify_center()
                            .h(px(theme.spacing.control_height))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_size(px(theme.typography.small_size))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.refresh_active_source(cx)
                            }))
                            .child("Refresh")
                            .when_some(
                                browser.ax_marker("browser-refresh-source"),
                                |element, marker| element.child(marker),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(
                        browser
                            .active_source_error()
                            .map(|_| theme.colors.danger)
                            .unwrap_or(theme.colors.text_muted),
                    ))
                    .child(browser.active_folder_label()),
            );
    let auxiliary = div()
        .flex()
        .flex_col()
        .min_h_0()
        .w_full()
        .debug_selector(|| String::from("sidebar-auxiliary"))
        .id("sidebar-auxiliary")
        .overflow_y_scroll()
        .child(curation::render_collection_filters(
            browser,
            theme.clone(),
            cx,
        ))
        .child(curation::render_playback_type_filters(
            browser,
            theme.clone(),
            cx,
        ))
        .child(curation::render_playback_age_filters(
            browser,
            theme.clone(),
            cx,
        ))
        .child(curation::render_rating_filters(browser, theme.clone(), cx))
        .child(super::curation_queue::render(browser, theme.clone(), cx))
        .child(tag_filter::render(browser, theme.clone(), cx))
        .child(tags::render(browser, theme.clone(), cx))
        .child(tag_completion::render(browser, theme.clone(), cx))
        .child(tag_chips::render(browser, theme.clone(), cx))
        .child(bottom_controls);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .min_h_0()
        .w(px(theme.spacing.sidebar_width))
        .h_full()
        .bg(color(theme.colors.surface_raised))
        .border_r(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(px(theme.spacing.control_height))
                .debug_selector(|| String::from("sidebar-source-header"))
                .px(px(theme.spacing.panel_padding))
                .bg(color(theme.colors.surface_overlay))
                .border_b(px(theme.spacing.border_width))
                .border_color(color(theme.colors.border))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child("SOURCES")
                .child(div().flex_1())
                .child(
                    div()
                        .id("add-source-sidebar")
                        .relative()
                        .when_some(browser.ax_focus("browser-add-source"), |element, focus| {
                            element.track_focus(&focus)
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(24.0))
                        .h(px(theme.spacing.control_height))
                        .cursor_pointer()
                        .text_size(px(theme.typography.title_size))
                        .text_color(color(theme.colors.accent))
                        .on_click(cx.listener(Browser::choose_folder))
                        .child("+")
                        .when_some(
                            browser.ax_marker("browser-add-source"),
                            |element, marker| element.child(marker),
                        ),
                ),
        )
        .child(source_list)
        .child(
            div()
                .flex_none()
                .px(px(theme.spacing.panel_padding))
                .py(px(theme.spacing.control_gap))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(active_label),
        )
        .when(active_error.is_some(), |element| {
            element.child(
                div()
                    .flex_none()
                    .px(px(theme.spacing.panel_padding))
                    .pb(px(theme.spacing.control_gap))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.danger))
                    .child(active_error.clone().unwrap_or_default()),
            )
        })
        .child(folder_list)
        .child(auxiliary)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::Browser;
    use gpui::{
        AvailableSpace, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext, point, px, size,
    };
    use yield_core::FolderTreeRow;

    fn draw_sidebar(browser: &gpui::Entity<Browser>, cx: &mut VisualTestContext) {
        cx.draw(
            point(px(0.0), px(0.0)),
            size(
                AvailableSpace::Definite(px(960.0)),
                AvailableSpace::Definite(px(560.0)),
            ),
            |_, _| browser.clone(),
        );
    }

    #[gpui::test]
    fn compact_sidebar_keeps_folder_space_and_scrolls_auxiliary_controls(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, _| assert_eq!(window.scale_factor(), 2.0));
        cx.simulate_resize(size(px(960.0), px(560.0)));
        browser.update(cx, |view, _| {
            view.folder_rows_cache = vec![FolderTreeRow {
                path: std::path::PathBuf::from("."),
                depth: 0,
                has_children: false,
                expanded: true,
                contains_audio: true,
                audio_file_count: 1,
            }];
        });

        draw_sidebar(&browser, cx);
        let theme = cx.update(|_, app| crate::theme::active(app).clone());
        let source_header = cx
            .debug_bounds("sidebar-source-header")
            .expect("source header bounds");
        let folder_list = cx
            .debug_bounds("sidebar-folder-list")
            .expect("folder list bounds");
        let auxiliary = cx
            .debug_bounds("sidebar-auxiliary")
            .expect("auxiliary bounds");
        let scope_before = cx
            .debug_bounds("folder-scope")
            .expect("scope bounds before scrolling");
        let refresh_before = cx
            .debug_bounds("refresh-source")
            .expect("refresh bounds before scrolling");

        assert_eq!(source_header.size.height, px(theme.spacing.control_height));
        assert!(folder_list.size.height >= px(theme.spacing.row_height * 4.0));
        assert!(scope_before.top() >= auxiliary.top());
        assert!(refresh_before.top() >= auxiliary.top());

        cx.simulate_event(ScrollWheelEvent {
            position: point(auxiliary.origin.x + px(8.0), auxiliary.origin.y + px(8.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-400.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        draw_sidebar(&browser, cx);

        let scope_after = cx
            .debug_bounds("folder-scope")
            .expect("scope bounds after scrolling");
        let refresh_after = cx
            .debug_bounds("refresh-source")
            .expect("refresh bounds after scrolling");
        assert!(
            scope_after.top() < scope_before.top(),
            "scope did not scroll: {scope_before:?} -> {scope_after:?}; auxiliary {auxiliary:?}"
        );
        assert!(
            refresh_after.top() < refresh_before.top(),
            "refresh did not scroll: {refresh_before:?} -> {refresh_after:?}; auxiliary {auxiliary:?}"
        );
        assert!(scope_after.bottom() <= auxiliary.bottom());
        assert!(refresh_after.bottom() <= auxiliary.bottom());
        assert!(scope_after.top() >= auxiliary.top());
        assert!(refresh_after.top() >= auxiliary.top());
    }
}
