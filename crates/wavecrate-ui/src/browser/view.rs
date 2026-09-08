#![deny(missing_docs)]
#![deny(warnings)]

//! Browser rendering and the compact Wavecrate application shell.

use gpui::{
    ClickEvent, Context, Div, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render,
    Window, div, font, prelude::*, px, uniform_list,
};
use std::time::{SystemTime, UNIX_EPOCH};
use wavecrate_core::{
    PlaybackType, SortColumn, SortDirection, format_duration, format_last_played,
};
use wavecrate_design::Theme;

use crate::theme::{active, color, transparent};

use super::Browser;
use super::context_menu;
use super::curation;
use super::display_labels;
use super::motion;
use super::normalized_audition;
use super::tag_filter;
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

fn current_unix_epoch_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

const MIN_SAMPLE_NAME_WIDTH: f32 = 192.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BrowserColumnVisibility {
    playback_type: bool,
    last_played: bool,
}

fn base_browser_min_window_width(theme: &Theme) -> f32 {
    theme.spacing.sidebar_width
        + theme.spacing.selected_marker_width
        + theme.spacing.rating_column_width
        + theme.spacing.collection_column_width
        + theme.spacing.extension_column_width
        + theme.spacing.size_column_width
        + theme.spacing.duration_column_width
        + MIN_SAMPLE_NAME_WIDTH
}

fn browser_column_visibility(window_width: f32, theme: &Theme) -> BrowserColumnVisibility {
    if !window_width.is_finite() {
        return BrowserColumnVisibility::default();
    }
    let base_width = base_browser_min_window_width(theme);
    let playback_type_width = theme.spacing.playback_type_column_width;
    let last_played_width = theme.spacing.last_played_column_width;
    let playback_type = window_width >= base_width + playback_type_width;
    let last_played = if playback_type {
        window_width >= base_width + playback_type_width + last_played_width
    } else {
        window_width >= base_width + last_played_width
    };
    BrowserColumnVisibility {
        playback_type,
        last_played,
    }
}

fn playback_type_label(playback_type: Option<PlaybackType>) -> &'static str {
    match playback_type {
        Some(PlaybackType::Loop) => "Loop",
        Some(PlaybackType::OneShot) => "1-Shot",
        None => "—",
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
            .w_full()
            .min_w_0()
            .h(px(theme.spacing.waveform_height))
            .bg(color(theme.colors.surface))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .w_full()
                    .min_w_0()
                    .h(px(theme.waveform.title_height))
                    .px(px(theme.spacing.panel_padding))
                    .bg(color(theme.colors.waveform_label))
                    .text_size(px(theme.typography.waveform_title_size))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(title),
                    ),
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
                    .w_full()
                    .min_w_0()
                    .h(px(theme.waveform.metadata_height))
                    .gap(px(theme.spacing.control_gap))
                    .px(px(theme.spacing.panel_padding))
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child(metadata)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_right()
                            .text_color(color(message.1))
                            .child(message.0),
                    ),
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
            .child(
                div()
                    .flex_initial()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label),
            )
            .when(selected, |header| {
                header.child(div().flex_none().whitespace_nowrap().child(marker))
            });
        if let Some(width) = width {
            header = header.flex_none().w(px(width));
        } else {
            header = header.flex_1();
        }
        header.on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
            view.toggle_sort(column, cx);
        }))
    }

    fn render_sample_name_view_mode_button(
        &self,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + 'static {
        let hover_fill = color(theme.interaction.hover_fill);
        let pressed_fill = color(theme.interaction.pressed_fill);
        let mode = self.sample_name_view_mode;
        div()
            .id("sample-name-view-mode")
            .flex()
            .flex_none()
            .items_center()
            .h(px(theme.spacing.control_height))
            .px(px(theme.spacing.control_gap))
            .cursor_pointer()
            .bg(color(theme.colors.surface_overlay))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .hover(move |style| style.bg(hover_fill))
            .active(move |style| style.bg(pressed_fill))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_muted))
            .child(mode.label())
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.toggle_sample_name_view_mode(cx);
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
        context_menu::invalidate_if_stale(self);
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
        } else if tag_filter::scope_pending(self) {
            tag_filter::status_label(self).unwrap_or_else(|| "Filtering tags…".to_string())
        } else if !self.entries.is_empty() && self.view_model.rows().len() != self.entries.len() {
            format!(
                "{} of {} audio files visible",
                self.view_model.rows().len(),
                self.entries.len()
            )
        } else {
            self.scan_status.clone()
        };
        let curation_status = self.active_curation_status();
        let selected_count = self.selected_count_label();
        let metadata_status = self.catalog_metadata.status_label();
        let display_labels_status = self.display_labels.status_label();
        let extension_width = theme.spacing.extension_column_width;
        let size_width = theme.spacing.size_column_width;
        let duration_width = theme.spacing.duration_column_width;
        let playback_type_width = theme.spacing.playback_type_column_width;
        let last_played_width = theme.spacing.last_played_column_width;
        let rating_width = theme.spacing.rating_column_width;
        let collection_width = theme.spacing.collection_column_width;
        let now_epoch_seconds = current_unix_epoch_seconds();
        let column_visibility =
            browser_column_visibility(f32::from(window.bounds().size.width), &theme);
        let show_playback_type = column_visibility.playback_type;
        let show_last_played = column_visibility.last_played;
        let toolbar_hover = theme.interaction.hover_fill;
        let toolbar_pressed = theme.interaction.pressed_fill;
        let loop_enabled = self.loop_toolbar_enabled();
        let loop_label = self.loop_toolbar_label();
        let normalized_audition_enabled = self.normalized_audition.enabled();
        let normalized_audition_preparing = self.normalized_audition_is_preparing();
        let normalized_audition_color = color(if normalized_audition_preparing {
            theme.colors.warning
        } else if normalized_audition_enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        if self.view_model.rows().is_empty()
            && let Some(context) = self.catalog_metadata.context()
        {
            cx.defer_in(window, move |view, _, cx| {
                view.observe_catalog_metadata(context, Vec::new(), cx);
            });
        }
        if self.view_model.rows().is_empty()
            && self.sample_name_view_mode == wavecrate_core::SampleNameViewMode::MetadataLabel
            && let Some(context) = self.display_labels.context()
        {
            cx.defer_in(window, move |view, _, cx| {
                view.observe_display_labels(context, Vec::new(), cx);
            });
        }
        let list = if self.view_model.rows().is_empty() {
            div()
                .flex()
                .flex_1()
                .justify_center()
                .items_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(
                    if self.entries.is_empty() || tag_filter::scope_pending(self) {
                        list_status.clone()
                    } else {
                        "No matching audio files".to_string()
                    },
                )
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
                            move |view, range: std::ops::Range<usize>, window, cx| {
                                if let Some(context) = view.catalog_metadata.context() {
                                    let metadata_paths = range
                                        .clone()
                                        .filter_map(|visible_index| {
                                            let row = view.view_model.rows().get(visible_index)?;
                                            let entry = view.entries.get(row.entry_index)?;
                                            (entry.duration_seconds.is_none()
                                                || entry.sample_rate.is_none())
                                            .then(|| entry.relative_path.clone())
                                        })
                                        .collect::<Vec<_>>();
                                    cx.defer_in(window, move |view, _, cx| {
                                        view.observe_catalog_metadata(context, metadata_paths, cx);
                                    });
                                }
                                if view.sample_name_view_mode
                                    == wavecrate_core::SampleNameViewMode::MetadataLabel
                                    && let Some(context) = view.display_labels.context()
                                {
                                    let label_paths = range
                                        .clone()
                                        .take(display_labels::MAX_VISIBLE_DEMAND_PATHS)
                                        .filter_map(|visible_index| {
                                            let row = view.view_model.rows().get(visible_index)?;
                                            let entry = view.entries.get(row.entry_index)?;
                                            Some(entry.relative_path.clone())
                                        })
                                        .collect::<Vec<_>>();
                                    cx.defer_in(window, move |view, _, cx| {
                                        view.observe_display_labels(context, label_paths, cx);
                                    });
                                }
                                let browser_focus_id = view.browser_focus_id();
                                range
                                    .map(|visible_index| {
                                        let row = view.view_model.rows()[visible_index].clone();
                                        let index = row.entry_index;
                                        let entry = view.entries[index].clone();
                                        let (sample_name, _) =
                                            view.sample_name_display(&entry.relative_path);
                                        let selected = view.sample_row_is_selected_with_focus(
                                            &row.id,
                                            browser_focus_id.as_ref(),
                                        );
                                        let current_sample =
                                            browser_focus_id.as_ref() == Some(&row.id);
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
                                        let playback_type = show_playback_type
                                            .then(|| playback_type_label(entry.playback_type));
                                        let last_played = show_last_played.then(|| {
                                            entry
                                                .curation
                                                .map(|curation| {
                                                    format_last_played(
                                                        curation.last_played_at(),
                                                        now_epoch_seconds,
                                                    )
                                                })
                                                .unwrap_or_else(|| "—".to_string())
                                        });
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
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                cx.listener(
                                                    move |view,
                                                          event: &MouseDownEvent,
                                                          window,
                                                          cx| {
                                                        view.open_context_menu(
                                                            index, event, window, cx,
                                                        );
                                                    },
                                                ),
                                            )
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
                                                        sample_name,
                                                    ),
                                            )
                                            .child(curation::rating_cell(
                                                &entry,
                                                &list_theme,
                                                rating_width,
                                            ))
                                            ;
                                        let sample_row = if let Some(playback_type) = playback_type
                                        {
                                            sample_row.child(
                                                div()
                                                    .flex_none()
                                                    .w(px(playback_type_width))
                                                    .px(px(list_theme.spacing.control_gap))
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(px(list_theme.typography.small_size))
                                                    .text_color(color(list_theme.colors.text_muted))
                                                    .child(playback_type),
                                            )
                                        } else {
                                            sample_row
                                        };
                                        let sample_row = sample_row
                                            .child(curation::collection_cell(
                                                &entry,
                                                &list_theme,
                                                collection_width,
                                            ))
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
                                            );
                                        let sample_row = if let Some(last_played) = last_played {
                                            sample_row.child(
                                                div()
                                                    .flex_none()
                                                    .w(px(last_played_width))
                                                    .px(px(list_theme.spacing.control_gap))
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(px(list_theme.typography.small_size))
                                                    .text_color(color(list_theme.colors.text_muted))
                                                    .child(last_played),
                                            )
                                        } else {
                                            sample_row
                                        };
                                        let sample_row = sample_row.child(
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
                                                .group_hover(row_group, |style| style.opacity(1.0)),
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
                            .text_color(color(if loop_enabled {
                                theme.colors.accent
                            } else {
                                theme.colors.text_muted
                            }))
                            .child(loop_label),
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
                            .id("normalized-audition-toolbar")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(28.0))
                            .h(px(theme.spacing.control_height))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .hover(move |style| style.bg(color(toolbar_hover)))
                            .active(move |style| style.bg(color(toolbar_pressed)))
                            .tooltip(normalized_audition::normalized_audition_tooltip)
                            .on_click(cx.listener(Self::toggle_normalized_audition_from_toolbar))
                            .child(normalized_audition::render_normalized_audition_icon(
                                normalized_audition_color,
                            )),
                    )
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
                    .min_w_0()
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
                            .min_w_0()
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
                                    .child({
                                        let mut column_header = div()
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
                                            .child(self.render_sample_name_view_mode_button(
                                                theme.clone(),
                                                cx,
                                            ))
                                            .child(self.render_column_header(
                                                "Rating",
                                                SortColumn::Rating,
                                                theme.clone(),
                                                cx,
                                                Some(rating_width),
                                                theme.spacing.control_gap,
                                            ));
                                        if show_playback_type {
                                            column_header =
                                                column_header.child(self.render_column_header(
                                                    "Type",
                                                    SortColumn::PlaybackType,
                                                    theme.clone(),
                                                    cx,
                                                    Some(playback_type_width),
                                                    theme.spacing.control_gap,
                                                ));
                                        }
                                        column_header =
                                            column_header.child(self.render_column_header(
                                                "Col",
                                                SortColumn::Collection,
                                                theme.clone(),
                                                cx,
                                                Some(collection_width),
                                                theme.spacing.control_gap,
                                            ));
                                        column_header =
                                            column_header.child(self.render_column_header(
                                                "Ext",
                                                SortColumn::Extension,
                                                theme.clone(),
                                                cx,
                                                Some(extension_width),
                                                theme.spacing.control_gap,
                                            ));
                                        column_header =
                                            column_header.child(self.render_column_header(
                                                "Size",
                                                SortColumn::Size,
                                                theme.clone(),
                                                cx,
                                                Some(size_width),
                                                theme.spacing.control_gap,
                                            ));
                                        column_header = column_header.child(
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
                                        );
                                        if show_last_played {
                                            column_header =
                                                column_header.child(self.render_column_header(
                                                    "Last Played",
                                                    SortColumn::LastPlayed,
                                                    theme.clone(),
                                                    cx,
                                                    Some(last_played_width),
                                                    theme.spacing.control_gap,
                                                ));
                                        }
                                        column_header
                                    })
                                    .child(list),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
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
                    .when(curation_status.is_some(), move |element| {
                        element.child(curation_status.clone().unwrap_or_default())
                    })
                    .when(metadata_status.is_some(), move |element| {
                        element.child(metadata_status.clone().unwrap_or_default())
                    })
                    .when(display_labels_status.is_some(), move |element| {
                        element.child(display_labels_status.clone().unwrap_or_default())
                    })
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
            .when(self.context_menu.is_some(), |element| {
                element.child(context_menu::render_overlay(self, &theme, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_columns_reserve_name_space_with_type_priority() {
        let theme = Theme::default();
        let base_width = base_browser_min_window_width(&theme);
        let type_width = theme.spacing.playback_type_column_width;
        let last_played_width = theme.spacing.last_played_column_width;
        assert_eq!(
            browser_column_visibility(base_width - 0.1, &theme),
            BrowserColumnVisibility::default()
        );
        assert_eq!(
            browser_column_visibility(base_width + type_width, &theme),
            BrowserColumnVisibility {
                playback_type: true,
                last_played: false,
            }
        );
        assert_eq!(
            browser_column_visibility(base_width + last_played_width, &theme),
            BrowserColumnVisibility {
                playback_type: true,
                last_played: false,
            }
        );
        assert_eq!(
            browser_column_visibility(base_width + type_width + last_played_width, &theme),
            BrowserColumnVisibility {
                playback_type: true,
                last_played: true,
            }
        );

        let mut legacy_theme = theme.clone();
        legacy_theme.spacing.sidebar_width = 398.0;
        assert_eq!(
            browser_column_visibility(960.0, &legacy_theme),
            BrowserColumnVisibility::default()
        );
        assert_eq!(
            browser_column_visibility(
                base_browser_min_window_width(&legacy_theme)
                    + legacy_theme.spacing.playback_type_column_width,
                &legacy_theme,
            ),
            BrowserColumnVisibility {
                playback_type: true,
                last_played: false,
            }
        );

        let mut wide_type_theme = theme.clone();
        wide_type_theme.spacing.playback_type_column_width = 240.0;
        assert_eq!(
            browser_column_visibility(
                base_browser_min_window_width(&wide_type_theme)
                    + wide_type_theme.spacing.last_played_column_width,
                &wide_type_theme,
            ),
            BrowserColumnVisibility {
                playback_type: false,
                last_played: true,
            }
        );
        assert_eq!(
            browser_column_visibility(f32::NAN, &theme),
            BrowserColumnVisibility::default()
        );
    }

    #[test]
    fn playback_type_labels_keep_unknown_and_recognized_values_compact() {
        assert_eq!(playback_type_label(Some(PlaybackType::Loop)), "Loop");
        assert_eq!(playback_type_label(Some(PlaybackType::OneShot)), "1-Shot");
        assert_eq!(playback_type_label(None), "—");
    }
}
