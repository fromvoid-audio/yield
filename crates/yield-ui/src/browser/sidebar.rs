#![deny(missing_docs)]
#![deny(warnings)]

//! Sources and folder-tree sidebar for the GPUI browser.

use std::path::PathBuf;

use gpui::{
    ClickEvent, Context, DragMoveEvent, Entity, MouseButton, MouseDownEvent, MouseUpEvent, Render,
    Window, div, prelude::*, px, uniform_list,
};
use yield_design::Theme;
use yield_library::sample_sources::SourceId;
use yield_session::RegistryRevision;

use crate::theme::{color, transparent};

use super::curation;
use super::motion::{self, MotionFrame};
use super::tag_filter;
use super::tag_library;
use super::{Browser, SourceReorderDrag};

#[derive(Clone)]
struct SourceDragPayload {
    browser: Entity<Browser>,
    source_id: SourceId,
    revision: RegistryRevision,
    label: String,
}

struct SourceDragGhost(String);

impl Render for SourceDragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(8.0))
            .py(px(4.0))
            .bg(color(theme.colors.surface_raised))
            .text_color(color(theme.colors.text_primary))
            .child(self.0.clone())
    }
}

impl Browser {
    fn begin_source_reorder_drag(&mut self, payload: &SourceDragPayload, cx: &mut Context<Self>) {
        if payload.browser.entity_id() != cx.entity().entity_id()
            || !self.source_registry_loaded
            || self.source_registry_revision != payload.revision
            || self.source_registry_pending_reorder.is_some()
            || !self.source_registry_pending_adds.is_empty()
            || !self.source_registry_pending_removes.is_empty()
            || !self
                .source_sessions
                .iter()
                .any(|session| session.source_id.as_ref() == Some(&payload.source_id))
        {
            self.source_reorder_drag = None;
            return;
        }
        self.source_reorder_drag = Some(SourceReorderDrag {
            source_id: payload.source_id.clone(),
            revision: payload.revision,
            target: None,
        });
        cx.notify();
    }

    fn update_source_reorder_drag(
        &mut self,
        payload: &SourceDragPayload,
        target_id: &SourceId,
        event: &DragMoveEvent<SourceDragPayload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.source_reorder_drag.as_mut() else {
            return;
        };
        if drag.source_id != payload.source_id
            || payload.browser.entity_id() != cx.entity().entity_id()
            || drag.revision != self.source_registry_revision
            || drag.revision != payload.revision
            || !event.bounds.contains(&window.mouse_position())
        {
            return;
        }
        let target = (
            target_id.clone(),
            window.mouse_position().y >= event.bounds.center().y,
        );
        if drag.target.as_ref() != Some(&target) {
            drag.target = Some(target);
            cx.notify();
        }
    }

    fn drop_source_reorder(
        &mut self,
        payload: &SourceDragPayload,
        target_id: &SourceId,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.source_reorder_drag.take() else {
            return;
        };
        let Some((hovered_id, after)) = drag.target else {
            cx.notify();
            return;
        };
        if drag.source_id != payload.source_id
            || payload.browser.entity_id() != cx.entity().entity_id()
            || drag.revision != payload.revision
            || drag.revision != self.source_registry_revision
            || hovered_id != *target_id
            || drag.source_id == hovered_id
            || !self
                .source_sessions
                .iter()
                .any(|session| session.source_id.as_ref() == Some(&hovered_id))
        {
            cx.notify();
            return;
        }
        let result = if cx.has_global::<super::source_registry::SourceRegistryGlobal>() {
            cx.update_global::<super::source_registry::SourceRegistryGlobal, _>(|global, _| {
                global.reorder_source(drag.source_id, hovered_id, after, drag.revision)
            })
        } else {
            Err(yield_session::SubmitError::Closed)
        };
        match result {
            Ok(request) => {
                self.source_registry_pending_reorder = Some((request, None));
                self.set_source_registry_status("Saving source order…".to_string());
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                self.set_source_registry_status(format!("Source reorder error: {error}"));
            }
        }
        cx.notify();
    }
}

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
    viewport_height: f32,
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
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _: &MouseUpEvent, _, cx| {
                    if view.source_reorder_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
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
                                            let source_id = view.source_sessions[index].source_id.clone();
                                            let drag_payload = source_id.as_ref().map(|source_id| SourceDragPayload {
                                                browser: cx.entity(),
                                                source_id: source_id.clone(),
                                                revision: view.source_registry_revision,
                                                label: label.clone(),
                                            });
                                            let drop_marker = view.source_reorder_drag.as_ref().and_then(|drag| {
                                                drag.target.as_ref().filter(|(target, _)| Some(target) == source_id.as_ref() && drag.source_id != *target)
                                            }).cloned();
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
                                                .when_some(source_id, |row, target_id| {
                                                    let move_id = target_id.clone();
                                                    row.on_drag_move::<SourceDragPayload>(cx.listener(
                                                        move |view, event: &DragMoveEvent<SourceDragPayload>, window, cx| {
                                                            view.update_source_reorder_drag(event.dragged_item().downcast_ref::<SourceDragPayload>().expect("source drag payload"), &move_id, event, window, cx);
                                                        },
                                                    ))
                                                    .on_drop(cx.listener(move |view, payload: &SourceDragPayload, _, cx| {
                                                        view.drop_source_reorder(payload, &target_id, cx);
                                                    }))
                                                })
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
                                                        .id(("source-drag-label", index))
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
                                                        .child(label)
                                                        .when_some(drag_payload, |element, drag| {
                                                            element.on_drag(drag, |drag, _, _, cx| {
                                                                let _ = drag.browser.update(cx, |view, cx| {
                                                                    view.begin_source_reorder_drag(drag, cx);
                                                                });
                                                                cx.new(|_| SourceDragGhost(drag.label.clone()))
                                                            })
                                                        }),
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
                                            let source_row = if let Some((_, after)) = drop_marker {
                                                source_row.child(
                                                    div()
                                                        .absolute()
                                                        .left_0()
                                                        .right_0()
                                                        .h(px(2.0))
                                                        .when(after, |marker| marker.bottom_0())
                                                        .when(!after, |marker| marker.top_0())
                                                        .bg(color(list_theme.colors.accent)),
                                                )
                                            } else {
                                                source_row
                                            };
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
                    .id("show-empty-folders")
                    .relative()
                    .when_some(
                        browser.ax_focus("browser-show-empty-folders"),
                        |element, focus| element.track_focus(&focus),
                    )
                    .debug_selector(|| String::from("show-empty-folders"))
                    .flex()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .h(px(theme.spacing.control_height))
                    .cursor_pointer()
                    .bg(color(theme.colors.surface_overlay))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(if browser.show_empty_folders {
                        theme.colors.accent
                    } else {
                        theme.colors.text_primary
                    }))
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.toggle_empty_folder_visibility(cx)
                    }))
                    .child(if browser.show_empty_folders {
                        "Hide Empty Folders"
                    } else {
                        "Show Empty Folders"
                    })
                    .when_some(
                        browser.ax_marker("browser-show-empty-folders"),
                        |element, marker| element.child(marker),
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
        .child(super::metadata_panel::render(
            browser,
            theme.clone(),
            viewport_height,
            cx,
        ))
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
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{super::SourceSession, Browser};
    use gpui::{
        AvailableSpace, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TestAppContext,
        TouchPhase, VisualTestContext, point, px, size,
    };
    use yield_core::{AudioFile, BrowserViewModel, FolderTree, FolderTreeRow};
    use yield_library::sample_sources::SourceId;
    use yield_media::enumerate_audio_files_with_cancel;

    fn audio(root: &Path, relative_path: &str) -> AudioFile {
        AudioFile {
            path: root.join(relative_path),
            relative_path: PathBuf::from(relative_path),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[gpui::test]
    fn hiding_selected_empty_folder_reconciles_to_visible_ancestor(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1858-empty-folder-fixture");
            let entries = vec![audio(&root, "kick.wav")];
            let tree = FolderTree::from_scan(&[PathBuf::from("empty")], &entries);
            let mut session = SourceSession::new(root.clone());
            session.entries = entries.clone();
            session.tree = tree.clone();
            session.selected_folder = PathBuf::from("empty");
            view.source_sessions = vec![session];
            view.active_source = Some(0);
            view.root = Some(root);
            view.entries = entries;
            view.folder_tree = Some(tree);
            view.selected_folder = PathBuf::from("empty");
            view.show_empty_folders = true;
            view.view_model = BrowserViewModel::new(&view.entries);
            view.view_model.set_folder_scope(
                &view.entries,
                Some(view.selected_folder.clone()),
                view.folder_scope,
            );
            view.rebuild_folder_rows_cache();
            assert!(
                view.folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("empty"))
            );

            view.toggle_empty_folder_visibility(cx);

            assert!(!view.show_empty_folders);
            assert_eq!(view.selected_folder, Path::new("."));
            assert_eq!(view.source_sessions[0].selected_folder, Path::new("."));
            assert!(
                !view
                    .folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("empty"))
            );
            assert!(view.selection.is_empty());
            assert!(
                view.selected_audio_path()
                    .is_none_or(|path| path == view.entries[0].path)
            );
        });
    }

    #[gpui::test]
    fn shown_empty_folder_can_be_selected(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.folder_tree = Some(FolderTree::from_scan(&[PathBuf::from("empty")], &[]));
            view.show_empty_folders = true;
            view.rebuild_folder_rows_cache();
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.select_folder(PathBuf::from("empty"), window, cx);
                assert_eq!(view.selected_folder, Path::new("empty"));
                assert!(view.view_model.rows().is_empty());
            });
        });
    }

    #[gpui::test]
    fn showing_empty_folders_keeps_cached_rows_while_refresh_starts(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1858-empty-folder-refresh-fixture");
            let entries = vec![audio(&root, "kick.wav")];
            let tree = FolderTree::from_scan(&[PathBuf::from("empty")], &entries);
            let mut session = SourceSession::new(root.clone());
            session.entries = entries.clone();
            session.tree = tree.clone();
            view.source_sessions = vec![session];
            view.active_source = Some(0);
            view.root = Some(root);
            view.entries = entries;
            view.folder_tree = Some(tree);
            view.view_model = BrowserViewModel::new(&view.entries);
            view.rebuild_folder_rows_cache();
            let visible_samples = view.view_model.rows().to_vec();
            assert!(
                !view
                    .folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("empty"))
            );

            view.toggle_empty_folder_visibility(cx);

            assert!(view.show_empty_folders);
            assert!(
                view.folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("empty"))
            );
            assert!(view.scan_task.is_some());
            assert!(view.scan_cancel.is_some());
            assert!(view.folder_tree.is_some());
            assert_eq!(view.view_model.rows(), visible_samples);
        });
    }

    #[gpui::test]
    fn enabling_empty_folders_refreshes_disk_only_directory_truth(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let directory = tempfile::tempdir().expect("temporary source");
        let root = fs::canonicalize(directory.path()).expect("canonical temporary source");
        let mut session = SourceSession::new(root.clone());
        session.source_id = Some(SourceId::from_string("opt-1858-refresh"));
        session.tree = FolderTree::from_scan(&[], &[]);
        fs::create_dir(root.join("new-empty-folder")).expect("new disk-only folder");

        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            view.source_sessions = vec![session];
            view.active_source = Some(0);
            view.root = Some(root);
            view.folder_tree = Some(view.source_sessions[0].tree.clone());
            view.rebuild_folder_rows_cache();
            assert!(
                !view.source_sessions[0]
                    .tree
                    .contains(Path::new("new-empty-folder"))
            );
            view.toggle_empty_folder_visibility(cx);
            assert!(
                view.folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("."))
            );
        });
        let scan = enumerate_audio_files_with_cancel(directory.path(), None)
            .expect("refresh includes new directory");
        browser.update(cx, |view, cx| {
            if let Some(cancellation) = view.scan_cancel.take() {
                cancellation.store(true, std::sync::atomic::Ordering::Release);
            }
            view.scan_task.take();
            view.scan_generation = view.scan_generation.wrapping_add(1);
            view.apply_scan_result(scan, Some(0), cx);
            assert!(
                view.source_sessions[0]
                    .tree
                    .contains(Path::new("new-empty-folder"))
            );
            assert!(
                view.folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("new-empty-folder"))
            );
        });
    }

    #[gpui::test]
    fn restored_source_snapshot_hides_empty_rows_and_reconciles_folder(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            let root = PathBuf::from("/tmp/opt1858-restored-snapshot");
            let entries = vec![audio(&root, "drums/kick.wav")];
            let tree = FolderTree::from_scan(&[PathBuf::from("empty")], &entries);
            let mut session = SourceSession::new(root.clone());
            session.entries = entries.clone();
            session.tree = tree.clone();
            session.selected_folder = PathBuf::from("empty");
            view.source_sessions = vec![session];
            view.active_source = Some(0);
            view.root = Some(root);
            view.entries = entries;
            view.folder_tree = Some(tree);
            view.selected_folder = PathBuf::from("empty");
            view.rebuild_folder_rows_cache();

            assert!(!view.show_empty_folders);
            assert!(view.source_sessions[0].tree.contains(Path::new("empty")));
            assert_eq!(view.selected_folder, Path::new("."));
            assert_eq!(view.source_sessions[0].selected_folder, Path::new("."));
            assert!(
                view.folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("drums"))
            );
            assert!(
                !view
                    .folder_rows()
                    .iter()
                    .any(|row| row.path == Path::new("empty"))
            );
        });
    }

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
        let empty_before = cx
            .debug_bounds("show-empty-folders")
            .expect("empty-folder toggle bounds before scrolling");

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
        let empty_after = cx
            .debug_bounds("show-empty-folders")
            .expect("empty-folder toggle bounds after scrolling");
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
        assert!(empty_after.top() < empty_before.top());
        assert!(empty_after.bottom() <= auxiliary.bottom());
        assert!(scope_after.top() >= auxiliary.top());
        assert!(refresh_after.top() >= auxiliary.top());
        assert!(empty_after.top() >= auxiliary.top());

        let center = point(
            empty_after.origin.x + empty_after.size.width / 2.0,
            empty_after.origin.y + empty_after.size.height / 2.0,
        );
        cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(center, MouseButton::Left, Modifiers::none());
        cx.cx
            .update(|app| assert!(browser.read(app).show_empty_folders));
    }
}
