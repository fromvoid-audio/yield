#![deny(missing_docs)]
#![deny(warnings)]

//! Browser rendering and the compact Wavecrate application shell.

use gpui::{
    ClickEvent, Context, Div, MouseButton, MouseMoveEvent, MouseUpEvent, Render, Window, div, font,
    prelude::*, px, uniform_list,
};
use wavecrate_core::{SortColumn, SortDirection, format_duration};
use wavecrate_design::Theme;

use crate::theme::{active, color, transparent};

use super::Browser;
use super::motion;
use super::volume;
use super::waveform;

fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MB", bytes as f32 / MIB as f32)
    } else if bytes >= KIB {
        format!("{} KB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

impl Browser {
    fn render_waveform_panel(
        &self,
        theme: Theme,
        playback_cursor_ratio: Option<f32>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title, metadata) = if let Some(index) = self.selected {
            if let Some(entry) = self.entries.get(index) {
                (
                    entry.relative_path.display().to_string(),
                    format!(
                        "{}  ·  {} bytes  ·  {} Hz",
                        format_duration(entry.duration_seconds),
                        entry.byte_len,
                        entry
                            .sample_rate
                            .map(|rate| rate.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    ),
                )
            } else {
                ("No sample selected".to_string(), String::new())
            }
        } else {
            ("No sample selected".to_string(), String::new())
        };
        let message = if let Some(error) = self.waveform_error.as_ref() {
            (error.clone(), theme.colors.danger)
        } else if let Some(preview) = self.waveform.as_ref() {
            (
                format!(
                    "Preview loaded: {}  ·  Space plays or stops  ·  Esc stops",
                    format_duration(Some(preview.duration_seconds))
                ),
                theme.colors.text_muted,
            )
        } else if self.selected.is_some() {
            ("Loading waveform…".to_string(), theme.colors.warning)
        } else {
            (
                "Select a sample to inspect its waveform".to_string(),
                theme.colors.text_muted,
            )
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .h(px(theme.spacing.waveform_height))
            .bg(color(theme.colors.surface))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(theme.waveform.title_height))
                    .px(px(theme.spacing.panel_padding))
                    .bg(color(theme.colors.waveform_label))
                    .text_size(px(theme.typography.waveform_title_size))
                    .child(title),
            )
            .child(waveform::render_waveform(
                self,
                theme.clone(),
                playback_cursor_ratio,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(theme.waveform.metadata_height))
                    .gap(px(theme.spacing.control_gap))
                    .px(px(theme.spacing.panel_padding))
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child(metadata)
                    .child(div().flex_1())
                    .child(div().text_color(color(message.1)).child(message.0)),
            )
    }

    fn render_column_header(
        &self,
        label: &'static str,
        column: SortColumn,
        theme: Theme,
        cx: &mut Context<Self>,
        width: Option<f32>,
        padding_left: f32,
    ) -> impl IntoElement + 'static {
        let sort = self.view_model.sort();
        let selected = sort.column == column;
        let marker = if selected {
            match sort.direction {
                SortDirection::Ascending => " ↑",
                SortDirection::Descending => " ↓",
            }
        } else {
            ""
        };
        let hover_fill = color(theme.interaction.hover_fill);
        let pressed_fill = color(theme.interaction.pressed_fill);
        let mut header = div()
            .id(label)
            .flex()
            .items_center()
            .h(px(theme.spacing.row_height))
            .pl(px(padding_left))
            .pr(px(theme.spacing.control_gap))
            .cursor_pointer()
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .hover(move |style| style.bg(hover_fill))
            .active(move |style| style.bg(pressed_fill))
            .text_size(px(theme.typography.small_size))
            .text_color(if selected {
                color(theme.colors.text_primary)
            } else {
                color(theme.colors.text_muted)
            })
            .child(format!("{label}{marker}"));
        if let Some(width) = width {
            header = header.flex_none().w(px(width));
        } else {
            header = header.flex_1();
        }
        header.on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
            view.toggle_sort(column, cx);
        }))
    }

    fn render_theme_settings(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let reload_pending = self.theme_load.pending_path.is_some();
        let settings_editable = self.preferences_editable();
        let reset_label = if settings_editable {
            "Default"
        } else {
            "Reset saved settings"
        };
        let reload_fill = if reload_pending || !settings_editable {
            color(theme.colors.control_disabled_fill)
        } else {
            transparent()
        };
        let reload_text = if reload_pending || !settings_editable {
            color(theme.colors.text_disabled)
        } else {
            color(theme.colors.text_primary)
        };
        let reload = div()
            .id("reload-theme")
            .flex()
            .items_center()
            .h(px(theme.spacing.control_height))
            .px(px(theme.spacing.control_gap))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .bg(reload_fill)
            .text_color(reload_text)
            .when(!reload_pending && settings_editable, |element| {
                element
                    .cursor_pointer()
                    .on_click(cx.listener(Self::reload_theme))
            })
            .child("Reload");
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(theme.spacing.control_gap))
            .h(px(
                theme.spacing.control_height + theme.spacing.panel_padding * 2.0
            ))
            .px(px(theme.spacing.panel_padding))
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_muted))
            .child("THEME")
            .child(
                div()
                    .id("choose-theme")
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .when(settings_editable, |element| {
                        element
                            .cursor_pointer()
                            .on_click(cx.listener(Self::choose_theme))
                    })
                    .text_color(if settings_editable {
                        color(theme.colors.text_primary)
                    } else {
                        color(theme.colors.text_disabled)
                    })
                    .child("Open"),
            )
            .child(reload)
            .child(
                div()
                    .id("reset-theme")
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .cursor_pointer()
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .on_click(cx.listener(Self::reset_theme))
                    .child(reset_label),
            )
            .child(
                div()
                    .id("toggle-animations")
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .when(settings_editable, |element| {
                        element
                            .cursor_pointer()
                            .on_click(cx.listener(Self::toggle_motion))
                    })
                    .text_color(if settings_editable {
                        color(if theme.motion.enabled {
                            theme.colors.accent
                        } else {
                            theme.colors.text_muted
                        })
                    } else {
                        color(theme.colors.text_disabled)
                    })
                    .child(if theme.motion.enabled {
                        "Animations On"
                    } else {
                        "Animations Off"
                    }),
            )
            .child(
                div()
                    .id("toggle-autoplay")
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .text_color(if settings_editable {
                        color(if self.autoplay_selection {
                            theme.colors.accent
                        } else {
                            theme.colors.text_muted
                        })
                    } else {
                        color(theme.colors.text_disabled)
                    })
                    .when(settings_editable, |element| {
                        element
                            .cursor_pointer()
                            .on_click(cx.listener(Self::toggle_autoplay))
                    })
                    .child(if self.autoplay_selection {
                        "Autoplay On"
                    } else {
                        "Autoplay Off"
                    }),
            )
            .child(div().flex_1())
            .child(format!(
                "{} · {}",
                self.theme_status,
                self.preference_status()
            ))
    }
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active(cx).clone();
        let motion_frame = self.motion_frame(theme.motion, window);
        let playback_cursor_ratio = self.playback_cursor_ratio(theme.motion.enabled);
        let root_label = self
            .root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No source folder".to_string());
        let source_error = self
            .active_source
            .and_then(|index| self.source_sessions.get(index))
            .and_then(|session| session.last_error.as_deref());
        let list_status = if let Some(error) = source_error {
            format!("Source error: {error}")
        } else if !self.entries.is_empty() && self.view_model.rows().len() != self.entries.len() {
            format!(
                "{} of {} audio files visible",
                self.view_model.rows().len(),
                self.entries.len()
            )
        } else {
            self.scan_status.clone()
        };
        let selected_count = self.selected_count_label();
        let extension_width = theme.spacing.extension_column_width;
        let size_width = theme.spacing.size_column_width;
        let duration_width = theme.spacing.duration_column_width;
        let toolbar_hover = theme.interaction.hover_fill;
        let toolbar_pressed = theme.interaction.pressed_fill;
        let list = if self.view_model.rows().is_empty() {
            div()
                .flex()
                .flex_1()
                .justify_center()
                .items_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(if self.entries.is_empty() {
                    list_status.clone()
                } else {
                    "No matching filenames".to_string()
                })
        } else {
            div()
                .flex_1()
                .h_full()
                .relative()
                .overflow_hidden()
                .child(
                    uniform_list(
                        "sample-list",
                        self.view_model.rows().len(),
                        cx.processor({
                            let list_theme = theme.clone();
                            move |view, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .map(|visible_index| {
                                        let row = view.view_model.rows()[visible_index].clone();
                                        let index = row.entry_index;
                                        let entry = view.entries[index].clone();
                                        let selected = view.sample_row_is_selected(index, &row.id);
                                        let current_sample = view.selected == Some(index);
                                        let focused = motion_frame.focus_alpha > 0
                                            && view.focus_matches_sample(&entry.path);
                                        let extension = entry
                                            .path
                                            .extension()
                                            .map(|extension| {
                                                extension.to_string_lossy().to_string()
                                            })
                                            .unwrap_or_default();
                                        let size = format_file_size(entry.byte_len);
                                        let duration = format_duration(entry.duration_seconds);
                                        let hover_fill =
                                            color(list_theme.colors.selection_hover_fill);
                                        let pressed_fill =
                                            color(list_theme.interaction.pressed_fill);
                                        let row_group =
                                            format!("sample-row:{}", entry.path.to_string_lossy());
                                        let sample_row = div()
                                            .group(row_group.clone())
                                            .id(index)
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
                                                color(list_theme.colors.surface)
                                            })
                                            .hover(move |style| style.bg(hover_fill))
                                            .active(move |style| style.bg(pressed_fill))
                                            .on_click(cx.listener(
                                                move |view, event: &ClickEvent, _, cx| {
                                                    view.select_entry_from_click(index, event, cx);
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(list_theme.spacing.selected_marker_width))
                                                    .h_full()
                                                    .bg(if selected || current_sample {
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
                                                    .pr(px(list_theme.spacing.control_gap))
                                                    .text_size(px(list_theme.typography.body_size))
                                                    .text_color(if selected {
                                                        color(list_theme.colors.accent)
                                                    } else {
                                                        color(list_theme.colors.text_primary)
                                                    })
                                                    .child(
                                                        entry.relative_path.display().to_string(),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(extension_width))
                                                    .px(px(list_theme.spacing.control_gap))
                                                    .text_size(px(list_theme.typography.small_size))
                                                    .text_color(color(list_theme.colors.text_muted))
                                                    .child(extension),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(size_width))
                                                    .px(px(list_theme.spacing.control_gap))
                                                    .text_size(px(list_theme.typography.small_size))
                                                    .text_color(color(list_theme.colors.text_muted))
                                                    .child(size),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(duration_width))
                                                    .px(px(list_theme.spacing.control_gap))
                                                    .text_size(px(list_theme.typography.small_size))
                                                    .text_color(color(list_theme.colors.text_muted))
                                                    .child(duration),
                                            )
                                            .child(
                                                div()
                                                    .absolute()
                                                    .top_0()
                                                    .bottom_0()
                                                    .right_0()
                                                    .w(px(super::motion::HOVER_RAIL_WIDTH))
                                                    .bg(super::motion::with_alpha(
                                                        list_theme.interaction.focus_outline,
                                                        super::motion::HOVER_RAIL_ALPHA,
                                                    ))
                                                    .opacity(0.0)
                                                    .group_hover(row_group, |style| {
                                                        style.opacity(1.0)
                                                    }),
                                            );
                                        if focused {
                                            sample_row.child(
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
                                            sample_row
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }
                        }),
                    )
                    .h_full()
                    .track_scroll(self.list_scroll_handle.clone()),
                )
                .child(motion::scroll_edge_fades(
                    self.list_scroll_handle.clone(),
                    self.sample_edge_transitions.clone(),
                    theme.motion,
                    self.motion_clock_origin,
                    theme.colors.surface,
                ))
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(color(theme.colors.background))
            .text_color(color(theme.colors.text_primary))
            .font(font(theme.typography.font_family.clone()))
            .line_height(px(theme.typography.line_height))
            .text_size(px(theme.typography.body_size))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                if view.update_waveform_pan_drag(
                    event.position,
                    event.pressed_button == Some(MouseButton::Middle),
                ) {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|view, _: &MouseUpEvent, _, cx| {
                    if view.finish_waveform_pan_drag() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|view, _: &MouseUpEvent, _, cx| {
                    if view.finish_waveform_pan_drag() {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(theme.spacing.toolbar_height))
                    .gap(px(theme.spacing.control_gap))
                    .px(px(theme.spacing.panel_padding))
                    .bg(color(theme.colors.surface_raised))
                    .border_b(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border))
                    .child(
                        div()
                            .flex_none()
                            .w(px((theme.spacing.toolbar_leading_inset
                                - theme.spacing.panel_padding)
                                .max(0.0)))
                            .h_full()
                            .on_mouse_down(MouseButton::Left, |_event, window, _cx| {
                                window.start_window_move();
                            }),
                    )
                    .child(
                        div()
                            .id("play-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(theme.spacing.control_height))
                            .h(px(theme.spacing.control_height))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .hover(move |style| style.bg(color(toolbar_hover)))
                            .active(move |style| style.bg(color(toolbar_pressed)))
                            .on_click(cx.listener(Self::toggle_playback_from_toolbar))
                            .text_size(px(theme.typography.title_size))
                            .text_color(color(theme.colors.accent))
                            .child("▶"),
                    )
                    .child(
                        div()
                            .id("stop-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(theme.spacing.control_height))
                            .h(px(theme.spacing.control_height))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .hover(move |style| style.bg(color(toolbar_hover)))
                            .active(move |style| style.bg(color(toolbar_pressed)))
                            .on_click(cx.listener(Self::stop_playback_from_toolbar))
                            .text_size(px(theme.typography.title_size))
                            .text_color(color(theme.colors.danger))
                            .child("■"),
                    )
                    .child(
                        div()
                            .id("loop-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .hover(move |style| style.bg(color(toolbar_hover)))
                            .active(move |style| style.bg(color(toolbar_pressed)))
                            .on_click(cx.listener(Self::toggle_loop_from_toolbar))
                            .text_size(px(theme.typography.small_size))
                            .text_color(color(if self.loop_enabled {
                                theme.colors.accent
                            } else {
                                theme.colors.text_muted
                            }))
                            .child(if self.playing_path.is_some() {
                                if self.loop_enabled {
                                    "Loop next: On"
                                } else {
                                    "Loop next: Off"
                                }
                            } else if self.loop_enabled {
                                "Loop On"
                            } else {
                                "Loop Off"
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(220.0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::invalidate_file_handoff_from_search_mouse),
                            )
                            .child(self.search_input.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(theme.typography.small_size))
                            .text_color(color(theme.colors.text_muted))
                            .child(root_label),
                    )
                    .child(volume::render_slider(self, theme.clone(), cx))
                    .child(
                        div()
                            .id("audio-output-settings-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .hover(move |style| style.bg(color(toolbar_hover)))
                            .text_size(px(theme.typography.small_size))
                            .text_color(color(if self.audio_output_error.is_some() {
                                theme.colors.danger
                            } else if self
                                .audio_output_resolved
                                .as_ref()
                                .is_some_and(|output| output.used_fallback)
                            {
                                theme.colors.warning
                            } else {
                                theme.colors.text_muted
                            }))
                            .on_click(cx.listener(Self::toggle_audio_output_settings))
                            .child(self.audio_output_toolbar_label()),
                    )
                    .child(
                        div()
                            .id("reveal-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_size(px(theme.typography.small_size))
                            .on_click(cx.listener(Self::reveal_focused_file))
                            .child("Reveal"),
                    )
                    .child(
                        div()
                            .id("choose-folder-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_size(px(theme.typography.small_size))
                            .on_click(cx.listener(Self::choose_folder))
                            .child("+"),
                    )
                    .child(
                        div()
                            .id("theme-settings")
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_size(px(theme.typography.small_size))
                            .on_click(cx.listener(Self::toggle_theme_settings))
                            .child("⚙"),
                    ),
            )
            .when(self.theme_settings_open, |element| {
                element.child(self.render_theme_settings(theme.clone(), cx))
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .h_full()
                    .child(super::sidebar::render(
                        self,
                        theme.clone(),
                        motion_frame,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .child(self.render_waveform_panel(
                                theme.clone(),
                                playback_cursor_ratio,
                                cx,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .h_full()
                                    .bg(color(theme.colors.surface))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_none()
                                            .items_center()
                                            .h(px(theme.spacing.row_height))
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(theme.spacing.selected_marker_width))
                                                    .h_full()
                                                    .bg(transparent()),
                                            )
                                            .child(self.render_column_header(
                                                "Name",
                                                SortColumn::Name,
                                                theme.clone(),
                                                cx,
                                                None,
                                                theme.spacing.row_padding_x,
                                            ))
                                            .child(self.render_column_header(
                                                "Ext",
                                                SortColumn::Extension,
                                                theme.clone(),
                                                cx,
                                                Some(extension_width),
                                                theme.spacing.control_gap,
                                            ))
                                            .child(self.render_column_header(
                                                "Size",
                                                SortColumn::Size,
                                                theme.clone(),
                                                cx,
                                                Some(size_width),
                                                theme.spacing.control_gap,
                                            ))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .flex_none()
                                                    .w(px(duration_width))
                                                    .h(px(theme.spacing.row_height))
                                                    .pl(px(theme.spacing.control_gap))
                                                    .pr(px(theme.spacing.control_gap))
                                                    .border_b(px(theme.spacing.border_width))
                                                    .border_color(color(theme.colors.border))
                                                    .text_size(px(theme.typography.small_size))
                                                    .text_color(color(theme.colors.text_muted))
                                                    .child("Duration"),
                                            ),
                                    )
                                    .child(list),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(theme.spacing.status_height))
                    .gap(px(theme.spacing.control_gap))
                    .px(px(theme.spacing.panel_padding))
                    .bg(color(theme.colors.surface_raised))
                    .border_t(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border))
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child(self.playback_status.clone())
                    .when(self.volume_status.is_some(), |element| {
                        element.child(self.volume_status.clone().unwrap_or_default())
                    })
                    .child(list_status)
                    .child(self.source_registry_status.clone())
                    .when(selected_count.is_some(), move |element| {
                        element.child(selected_count.unwrap_or_default())
                    })
                    .child(div().flex_1())
                    .child(self.theme_status.clone())
                    .when(self.motion_pending().any(), |element| {
                        element.child(
                            div()
                                .flex_none()
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded_full()
                                .bg(color(theme.colors.accent))
                                .opacity(f32::from(motion_frame.worker_alpha) / 255.0),
                        )
                    }),
            )
            .when(self.audio_settings_open, |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(theme.spacing.toolbar_height))
                        .right_0()
                        .w(px(420.0))
                        .occlude()
                        .child(self.render_audio_output_settings(theme.clone(), cx)),
                )
            })
    }
}
