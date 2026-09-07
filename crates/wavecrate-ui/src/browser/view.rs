#![deny(missing_docs)]
#![deny(warnings)]

//! Browser rendering and the compact Wavecrate application shell.

use gpui::{
    ClickEvent, Context, Div, MouseButton, Render, Window, div, font, prelude::*, px, uniform_list,
};
use wavecrate_core::{SortColumn, SortDirection, format_duration};
use wavecrate_design::Theme;

use crate::theme::{active, color, transparent};

use super::Browser;
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
    fn render_waveform_panel(&self, theme: Theme) -> impl IntoElement {
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
            (
                "Loading bounded waveform preview…".to_string(),
                theme.colors.warning,
            )
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
            .child(waveform::render_waveform(self, theme.clone()))
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
        let reload_fill = if reload_pending {
            color(theme.colors.control_disabled_fill)
        } else {
            transparent()
        };
        let reload_text = if reload_pending {
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
            .when(!reload_pending, |element| {
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
                    .cursor_pointer()
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .on_click(cx.listener(Self::choose_theme))
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
                    .child("Default"),
            )
            .child(div().flex_1())
            .child(self.theme_status.clone())
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active(cx).clone();
        let root_label = self
            .root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No source folder".to_string());
        let list_status =
            if !self.entries.is_empty() && self.view_model.rows().len() != self.entries.len() {
                format!(
                    "{} of {} audio files visible",
                    self.view_model.rows().len(),
                    self.entries.len()
                )
            } else {
                self.scan_status.clone()
            };
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
            div().flex_1().h_full().child(
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
                                    let selected = view.selected == Some(index);
                                    let extension = entry
                                        .path
                                        .extension()
                                        .map(|extension| extension.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    let size = format_file_size(entry.byte_len);
                                    let duration = format_duration(entry.duration_seconds);
                                    let hover_fill = color(list_theme.colors.selection_hover_fill);
                                    let pressed_fill = color(list_theme.interaction.pressed_fill);
                                    div()
                                        .id(index)
                                        .flex()
                                        .flex_none()
                                        .w_full()
                                        .items_center()
                                        .h(px(list_theme.spacing.row_height))
                                        .cursor_pointer()
                                        .bg(if selected {
                                            color(list_theme.colors.selection_fill)
                                        } else {
                                            color(list_theme.colors.surface)
                                        })
                                        .hover(move |style| style.bg(hover_fill))
                                        .active(move |style| style.bg(pressed_fill))
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                view.select_entry(index, cx);
                                            },
                                        ))
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
                                                .child(entry.relative_path.display().to_string()),
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
                                })
                                .collect::<Vec<_>>()
                        }
                    }),
                )
                .h_full()
                .track_scroll(self.list_scroll_handle.clone()),
            )
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(color(theme.colors.background))
            .text_color(color(theme.colors.text_primary))
            .font(font(theme.typography.font_family.clone()))
            .line_height(px(theme.typography.line_height))
            .text_size(px(theme.typography.body_size))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
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
                            .flex_none()
                            .w(px(220.0))
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
                    .child(super::sidebar::render(self, theme.clone(), cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .child(self.render_waveform_panel(theme.clone()))
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
                    .child(list_status)
                    .child(div().flex_1())
                    .child(self.theme_status.clone()),
            )
    }
}
