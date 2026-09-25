#![deny(missing_docs)]
#![deny(warnings)]

//! Browser rendering and the compact Yield application shell.

use gpui::{
    ClickEvent, Context, Div, DragMoveEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Render, Window, div, font, prelude::*, px, uniform_list,
};
use std::time::{SystemTime, UNIX_EPOCH};
use yield_core::{PlaybackType, SortColumn, SortDirection, format_duration, format_last_played};
use yield_design::Theme;

use crate::theme::{active, color, transparent};

use super::Browser;
use super::context_menu;
use super::curation;
use super::display_labels;
use super::file_drag;
use super::metronome;
use super::motion;
use super::normalized_audition;
use super::random_audition;
use super::shortcut_help;
use super::similar_sections;
use super::tag_filter;
use super::volume;
use super::waveform;
use super::zero_crossing;

struct RandomNavigationTooltip;

impl Render for RandomNavigationTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_primary))
            .child("Random sample navigation: Down visits an unvisited row; Up follows history.")
    }
}

fn random_navigation_tooltip(_: &mut Window, cx: &mut gpui::App) -> gpui::AnyView {
    cx.new(|_| RandomNavigationTooltip).into()
}

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
pub(super) struct BrowserColumnVisibility {
    pub(super) playback_type: bool,
    pub(super) last_played: bool,
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

pub(super) fn browser_column_visibility(
    window_width: f32,
    theme: &Theme,
) -> BrowserColumnVisibility {
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
            .when_some(
                self.ax_focus(super::accessibility::sort_id(column)),
                |element, focus| element.track_focus(&focus),
            )
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
            })
            .when_some(
                self.ax_marker(super::accessibility::sort_id(column)),
                |element, marker| element.child(marker),
            );
        if let Some(width) = width {
            header = header.flex_none().w(px(width));
        } else {
            header = header.flex_1();
        }
        header.on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
            view.toggle_sort(column, cx);
        }))
    }

    fn render_random_navigation_button(
        &self,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + 'static {
        let hover_fill = color(theme.interaction.hover_fill);
        let pressed_fill = color(theme.interaction.pressed_fill);
        let foreground = color(if self.random_navigation.enabled() {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        div()
            .id("random-navigation-toggle")
            .when_some(
                self.ax_focus(super::accessibility::RANDOM_NAVIGATION),
                |element, focus| element.track_focus(&focus),
            )
            .debug_selector(|| String::from("random-navigation-toggle"))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(px(28.0))
            .h(px(theme.spacing.control_height))
            .cursor_pointer()
            .bg(color(theme.colors.surface_overlay))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .hover(move |style| style.bg(hover_fill))
            .active(move |style| style.bg(pressed_fill))
            .text_size(px(theme.typography.small_size))
            .text_color(foreground)
            .when(self.hover_help_visible(), |control| {
                control.tooltip(random_navigation_tooltip)
            })
            .child("⚄")
            .when_some(
                self.ax_marker(super::accessibility::RANDOM_NAVIGATION),
                |element, marker| element.child(marker),
            )
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.toggle_random_navigation(cx);
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
            .when_some(
                self.ax_focus(super::accessibility::NAME_VIEW_MODE),
                |element, focus| element.track_focus(&focus),
            )
            .debug_selector(|| String::from("sample-name-view-mode"))
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
            .when_some(
                self.ax_marker(super::accessibility::NAME_VIEW_MODE),
                |element, marker| element.child(marker),
            )
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.toggle_sample_name_view_mode(cx);
            }))
    }

    fn render_theme_settings(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let enlarged =
            self.general_settings.text_scale_percent == yield_session::ENLARGED_TEXT_SCALE_PERCENT;
        let focus_color = color(theme.interaction.focus_outline);
        let focus_width = theme.interaction.focus_outline_width;
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
            .flex_none()
            .relative()
            .when_some(self.ax_focus("settings-theme-reload"), |element, focus| {
                element.track_focus(&focus)
            })
            .when_some(
                self.ax_marker("settings-theme-reload"),
                |element, marker| element.child(marker),
            )
            .flex()
            .items_center()
            .h(px(theme.spacing.control_height))
            .px(px(theme.spacing.control_gap))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
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
            .flex_wrap()
            .flex_none()
            .debug_selector(|| String::from("theme-settings-strip"))
            .w_full()
            .min_w_0()
            .items_center()
            .gap(px(theme.spacing.control_gap))
            .min_h(px(
                theme.spacing.control_height + theme.spacing.panel_padding * 2.0
            ))
            .px(px(theme.spacing.panel_padding))
            .py(px(theme.spacing.panel_padding))
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_muted))
            .child("THEME")
            .child(
                div()
                    .id("choose-theme")
                    .flex_none()
                    .relative()
                    .when_some(self.ax_focus("settings-theme-choose"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(
                        self.ax_marker("settings-theme-choose"),
                        |element, marker| element.child(marker),
                    )
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
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
                    .flex_none()
                    .relative()
                    .when_some(self.ax_focus("settings-theme-reset"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(self.ax_marker("settings-theme-reset"), |element, marker| {
                        element.child(marker)
                    })
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .cursor_pointer()
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
                    .on_click(cx.listener(Self::reset_theme))
                    .child(reset_label),
            )
            .child(
                div()
                    .id("toggle-animations")
                    .flex_none()
                    .relative()
                    .when_some(self.ax_focus("settings-motion"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(self.ax_marker("settings-motion"), |element, marker| {
                        element.child(marker)
                    })
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
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
                    .id("toggle-text-scale")
                    .flex_none()
                    .debug_selector(|| String::from("toggle-text-scale"))
                    .relative()
                    .when_some(self.ax_focus("settings-text-scale"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(self.ax_marker("settings-text-scale"), |element, marker| {
                        element.child(marker)
                    })
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
                    .when(settings_editable, |element| {
                        element
                            .cursor_pointer()
                            .on_click(cx.listener(Self::toggle_text_scale))
                    })
                    .text_color(if settings_editable {
                        color(theme.colors.text_primary)
                    } else {
                        color(theme.colors.text_disabled)
                    })
                    .child(format!(
                        "Text {}%",
                        self.general_settings.text_scale_percent
                    )),
            )
            .child(
                div()
                    .id("toggle-autoplay")
                    .flex_none()
                    .relative()
                    .when_some(self.ax_focus("settings-autoplay"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(self.ax_marker("settings-autoplay"), |element, marker| {
                        element.child(marker)
                    })
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
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
            .child(
                div()
                    .id("toggle-help-tooltips")
                    .flex_none()
                    .debug_selector(|| String::from("toggle-help-tooltips"))
                    .relative()
                    .when_some(self.ax_focus("settings-help-tips"), |element, focus| {
                        element.track_focus(&focus)
                    })
                    .when_some(self.ax_marker("settings-help-tips"), |element, marker| {
                        element.child(marker)
                    })
                    .flex()
                    .items_center()
                    .h(px(theme.spacing.control_height))
                    .px(px(theme.spacing.control_gap))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border_emphasis))
                    .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
                    .text_color(if settings_editable {
                        color(if self.help_tooltips_enabled {
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
                            .on_click(cx.listener(Self::toggle_help_tooltips))
                    })
                    .child(if self.help_tooltips_enabled {
                        "Help tips On"
                    } else {
                        "Help tips Off"
                    }),
            )
            .child(div().when(!enlarged, |spacer| spacer.flex_1()))
            .child(
                div()
                    .relative()
                    .when(enlarged, |status| status.w_full().flex_none())
                    .when_some(
                        self.ax_marker("settings-theme-status"),
                        |element, marker| element.child(marker),
                    )
                    .child(format!(
                        "{} · {}",
                        self.theme_status,
                        self.preference_status()
                    )),
            )
    }
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        self.sync_browser_accessibility(window, cx);
        if self.playmark_timing_blur_subscription.is_none() {
            let focus_handle = self.playmark_timing_input.read(cx).focus_handle();
            self.playmark_timing_blur_subscription =
                Some(cx.on_blur(&focus_handle, window, |view, window, cx| {
                    view.commit_playmark_timing_edit(window, cx, false);
                }));
        }
        context_menu::invalidate_if_stale(self);
        similar_sections::invalidate_if_stale(self);
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
        let zero_crossing_status = self.zero_crossing.status_label();
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
        let toolbar_gap = if f32::from(window.bounds().size.width) < 1_120.0 {
            theme.spacing.control_gap.min(4.0)
        } else {
            theme.spacing.control_gap
        };
        let loop_enabled = self.loop_toolbar_enabled();
        let loop_label = self.loop_toolbar_label();
        let bpm_snap_enabled = self.bpm_snap_enabled;
        let bpm_snap_color = color(if bpm_snap_enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        let zero_crossing_enabled = self.zero_crossing.enabled();
        let zero_crossing_color = color(if zero_crossing_enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        let beat_guides_enabled = self.beat_guides_enabled;
        let beat_guides_color = color(if beat_guides_enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        let random_color = color(if self.random_audition_sticky {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
        let metronome_enabled = self.metronome_toolbar_enabled();
        let metronome_pending = self.metronome_toolbar_pending();
        let metronome_color = color(if metronome_pending {
            theme.colors.warning
        } else if metronome_enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        });
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
            && self.sample_name_view_mode == yield_core::SampleNameViewMode::MetadataLabel
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
                                #[cfg(target_os = "macos")]
                                view.update_ax_sample_range(range.clone(), window, cx);
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
                                    == yield_core::SampleNameViewMode::MetadataLabel
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
                                        let sample_ax_id = view.ax_sample_id(&entry.path);
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
                                            .when_some(sample_ax_id.as_deref().and_then(|id| view.ax_focus(id)), |element, focus| element.track_focus(&focus))
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
                                        )
                                        .when_some(sample_ax_id.and_then(|id| view.ax_marker(id)), |element, marker| element.child(marker));
                                        let drag = view.file_drag_row_payload(
                                            cx.entity(),
                                            visible_index,
                                            &row,
                                        );
                                        let sample_row = sample_row.when_some(Some(drag), |this, drag| {
                                            this.on_drag(drag, |drag, _, window, cx| {
                                                let _ = drag.browser.update(cx, |view, browser_cx| {
                                                    view.begin_file_drag(
                                                        drag.clone(), window, browser_cx,
                                                    );
                                                });
                                                cx.new(|_| {
                                                    file_drag::FileDragGhost::new(drag.preview_count)
                                                })
                                            })
                                            .on_drag_move::<file_drag::FileDragRowPayload>(
                                                cx.listener(
                                                    |view,
                                                     event: &DragMoveEvent<
                                                         file_drag::FileDragRowPayload,
                                                     >,
                                                     window,
                                                     cx| {
                                                        view.on_file_drag_move(event, window, cx);
                                                    },
                                                ),
                                            )
                                        });
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
            .capture_any_mouse_down(cx.listener(Self::cancel_file_drag_from_mouse_down))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                let region_changed =
                    view.update_waveform_region_drag(event.position, event.pressed_button);
                if region_changed
                    && view
                        .waveform_region_drag
                        .is_some_and(|drag| drag.button() == MouseButton::Right)
                {
                    zero_crossing::schedule_for_drag(view, cx);
                }
                let pan_changed = view.update_waveform_pan_drag(
                    event.position,
                    event.pressed_button == Some(MouseButton::Middle),
                );
                if region_changed || pan_changed {
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
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    view.finish_waveform_region_drag(event.position, MouseButton::Right, cx);
                    // Match clicks commit on press without a region drag. Native secondary
                    // release must still invalidate the view to publish that selection.
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    view.finish_waveform_region_drag(event.position, MouseButton::Right, cx);
                    // Match clicks commit on press without a region drag. Native secondary
                    // release must still invalidate the view to publish that selection.
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(Self::finish_file_drag_pointer),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(Self::finish_file_drag_pointer),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(theme.spacing.toolbar_height))
                    .gap(px(toolbar_gap))
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
                            .relative()
                            .when_some(self.ax_focus("transport-play"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("transport-play"), |element, marker| {
                                element.child(marker)
                            })
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
                            .relative()
                            .when_some(self.ax_focus("transport-stop"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("transport-stop"), |element, marker| {
                                element.child(marker)
                            })
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
                            .relative()
                            .when_some(self.ax_focus("transport-loop"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("transport-loop"), |element, marker| {
                                element.child(marker)
                            })
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
                            .id("similar-sections-toolbar")
                            .relative()
                            .when_some(
                                self.ax_focus("transport-similar-sections"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .when_some(
                                self.ax_marker("transport-similar-sections"),
                                |element, marker| element.child(marker),
                            )
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(similar_sections::tooltip)
                            })
                            .on_click(
                                cx.listener(|view, _, _, cx| similar_sections::toggle(view, cx)),
                            )
                            .child(similar_sections::render_icon(color(
                                if self.similar_sections.pending() {
                                    theme.colors.warning
                                } else if self.similar_sections.enabled() {
                                    theme.colors.accent
                                } else {
                                    theme.colors.text_muted
                                },
                            ))),
                    )
                    .child(
                        div()
                            .id("bpm-snap-toolbar")
                            .relative()
                            .when_some(self.ax_focus("transport-bpm-snap"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("transport-bpm-snap"), |element, marker| {
                                element.child(marker)
                            })
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(waveform::bpm_snap_tooltip)
                            })
                            .on_click(cx.listener(Self::toggle_bpm_snap_from_toolbar))
                            .child(waveform::render_bpm_snap_icon(bpm_snap_color)),
                    )
                    .child(
                        div()
                            .id("zero-crossing-toolbar")
                            .relative()
                            .when_some(
                                self.ax_focus("transport-zero-crossing"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .when_some(
                                self.ax_marker("transport-zero-crossing"),
                                |element, marker| element.child(marker),
                            )
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(zero_crossing::tooltip)
                            })
                            .on_click(cx.listener(Self::toggle_zero_crossing_from_toolbar))
                            .child(zero_crossing::render_icon(zero_crossing_color)),
                    )
                    .child(
                        div()
                            .id("beat-guides-toolbar")
                            .relative()
                            .when_some(self.ax_focus("transport-beat-guides"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(
                                self.ax_marker("transport-beat-guides"),
                                |element, marker| element.child(marker),
                            )
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(waveform::beat_guides_tooltip)
                            })
                            .on_click(cx.listener(Self::toggle_beat_guides_from_toolbar))
                            .child(waveform::render_beat_guides_icon(beat_guides_color)),
                    )
                    .child(
                        div()
                            .id("bpm-beat-count-toolbar")
                            .relative()
                            .when_some(self.ax_marker("transport-beat-count"), |element, marker| {
                                element.child(marker)
                            })
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .w(px(34.0))
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(waveform::beat_count_tooltip)
                            })
                            .child(self.beat_count_input.clone()),
                    )
                    .child(
                        div()
                            .id("metronome-toolbar")
                            .relative()
                            .when_some(self.ax_focus("transport-metronome"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("transport-metronome"), |element, marker| {
                                element.child(marker)
                            })
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(metronome::tooltip)
                            })
                            .on_click(cx.listener(Self::toggle_metronome_from_toolbar))
                            .child(metronome::render_icon(metronome_color)),
                    )
                    .child(
                        div()
                            .id("random-audition-toolbar")
                            .relative()
                            .when_some(
                                self.ax_focus("transport-random-audition"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .when_some(
                                self.ax_marker("transport-random-audition"),
                                |element, marker| element.child(marker),
                            )
                            .debug_selector(|| String::from("random-audition-toolbar"))
                            .flex_none()
                            .flex()
                            .opacity(if self.random_audition_available() {
                                1.0
                            } else {
                                0.45
                            })
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(random_audition::tooltip)
                            })
                            .on_click(cx.listener(Self::random_audition_from_toolbar))
                            .child(random_audition::render_icon(random_color)),
                    )
                    .child(
                        div()
                            .min_w(px(80.0))
                            .w(px(220.0))
                            .relative()
                            .overflow_hidden()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::invalidate_file_handoff_from_search_mouse),
                            )
                            .child(self.search_input.clone())
                            .when_some(self.ax_marker("browser-search"), |element, marker| {
                                element.child(marker)
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
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
                            .relative()
                            .when_some(
                                self.ax_focus("transport-normalized-audition"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .when_some(
                                self.ax_marker("transport-normalized-audition"),
                                |element, marker| element.child(marker),
                            )
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(normalized_audition::normalized_audition_tooltip)
                            })
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
                            .relative()
                            .when_some(self.ax_focus("settings-open"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(self.ax_marker("settings-open"), |element, marker| {
                                element.child(marker)
                            })
                            .debug_selector(|| String::from("theme-settings"))
                            .flex_none()
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .bg(color(theme.colors.surface_overlay))
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .focus({
                                let focus_color = color(theme.interaction.focus_outline);
                                let focus_width = theme.interaction.focus_outline_width;
                                move |style| style.border(px(focus_width)).border_color(focus_color)
                            })
                            .text_size(px(theme.typography.small_size))
                            // Keep the input focused until the click handler records it for
                            // General Settings' Escape restoration. Without this, GPUI's
                            // focus-tracked Browser root claims focus for the pointer press.
                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.prevent_default();
                            })
                            .on_click(cx.listener(Self::toggle_theme_settings))
                            .child("⚙"),
                    )
                    .child(
                        div()
                            .id("shortcut-help-toolbar")
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
                            .when(self.hover_help_visible(), |control| {
                                control.tooltip(shortcut_help::shortcut_help_tooltip)
                            })
                            .on_click(cx.listener(Self::toggle_shortcut_help_from_toolbar))
                            .text_size(px(theme.typography.title_size))
                            .text_color(color(theme.colors.text_muted))
                            .child("?"),
                    ),
            )
            .when(self.theme_settings_open, |element| {
                element
                    .child(self.render_theme_settings(theme.clone(), cx))
                    .child(self.render_general_settings(theme.clone(), cx))
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
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
                                        let mut column_header =
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
                                                .child(self.render_random_navigation_button(
                                                    theme.clone(),
                                                    cx,
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
                    .when_some(self.compare_anchor.as_ref(), |element, anchor| {
                        let path = anchor.path();
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        element.child(format!("Compare: {name}"))
                    })
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
                    .when(zero_crossing_status.is_some(), move |element| {
                        element.child(zero_crossing_status.unwrap_or_default())
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
            .when(self.shortcut_help.is_open(), |element| {
                element.child(shortcut_help::render(
                    self,
                    theme.clone(),
                    window.bounds().size,
                    cx,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use gpui::{
        EntityInputHandler, KeyDownEvent, Keystroke, Modifiers, MouseDownEvent, TestAppContext,
        VisualTestContext, point, px,
    };
    use yield_core::{AudioFile, BrowserViewModel, WaveformEditFade, WaveformEditSettings};
    use yield_media::WaveformPreview;

    use super::super::transport;

    fn inert_preview() -> WaveformPreview {
        WaveformPreview {
            peaks: vec![0.5; 8],
            bands: Vec::new(),
            duration_seconds: 1.0,
            sample_rate: 48_000,
            total_frames: 48_000,
            detail_context: None,
        }
    }

    fn install_playmark_state(
        browser: &gpui::Entity<Browser>,
        cx: &mut VisualTestContext,
        path: &Path,
        region: waveform::WaveformPlayRegion,
    ) {
        browser.update(cx, |view, _| {
            view.root = path.parent().map(Path::to_path_buf);
            view.entries = vec![AudioFile {
                path: path.to_path_buf(),
                relative_path: path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("sample.wav")),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            view.waveform = Some(inert_preview());
            view.waveform_generation = 7;
            view.waveform_play_region = Some(region);
        });
    }

    fn commit_playmark_draft(
        browser: &gpui::Entity<Browser>,
        cx: &mut VisualTestContext,
        draft: &str,
    ) {
        let draft = draft.to_owned();
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content(draft, cx));
                view.commit_playmark_timing_edit(window, cx, true);
            });
        });
    }

    #[gpui::test]
    fn browser_selection_commands_target_visible_identities(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1763-selection-fixture");
            view.root = Some(root.clone());
            view.entries = ["kick.wav", "snare.wav"]
                .into_iter()
                .map(|name| AudioFile {
                    path: root.join(name),
                    relative_path: PathBuf::from(name),
                    byte_len: 0,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                })
                .collect();
            view.view_model = BrowserViewModel::new(&view.entries);
            view.selected = Some(0);
            let focused = view.entries[0].stable_id();
            let other = view.entries[1].stable_id();

            view.select_all_visible(cx);
            assert_eq!(
                view.selection
                    .selected_in_order(&view.visible_selection_ids()),
                [focused.clone(), other.clone()]
            );
            view.toggle_focused_selection(cx);
            assert_eq!(view.browser_focus_id(), Some(focused));
            assert_eq!(
                view.selection
                    .selected_in_order(&view.visible_selection_ids()),
                [other]
            );
        });
    }

    #[gpui::test]
    fn random_navigation_toggle_sits_beside_name_mode(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("refresh browser header");
        cx.run_until_parked();
        let toggle = cx
            .debug_bounds("random-navigation-toggle")
            .expect("random navigation toggle");
        let name_mode = cx
            .debug_bounds("sample-name-view-mode")
            .expect("sample name mode");
        assert!(toggle.origin.x + toggle.size.width <= name_mode.origin.x);

        let center = point(
            toggle.origin.x + toggle.size.width / 2.0,
            toggle.origin.y + toggle.size.height / 2.0,
        );
        cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(center, MouseButton::Left, Modifiers::none());
        cx.cx
            .update(|app| assert!(browser.read(app).random_navigation.enabled()));
    }

    #[gpui::test]
    fn random_navigation_uses_visible_visits_then_restores_linear_movement(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1859-random-navigation-fixture");
            view.root = Some(root.clone());
            view.entries = ["a.wav", "b.wav", "c.wav"]
                .into_iter()
                .map(|name| AudioFile {
                    path: root.join(name),
                    relative_path: PathBuf::from(name),
                    byte_len: 0,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                })
                .collect();
            view.view_model = BrowserViewModel::new(&view.entries);
            view.selected = Some(0);
            let first = view.entries[0].stable_id();

            view.toggle_random_navigation(cx);
            view.selection.select_all(&view.visible_selection_ids());
            view.move_selection(1, false, false, true, cx);
            let second = view.browser_focus_id().expect("first random jump");
            assert_ne!(second, first);
            assert!(view.selection.is_empty());
            view.move_selection(1, false, false, true, cx);
            let third = view.browser_focus_id().expect("second random jump");
            assert_ne!(third, first);
            assert_ne!(third, second);

            view.move_selection(-1, false, false, true, cx);
            assert_eq!(view.browser_focus_id(), Some(second.clone()));
            view.root = Some(PathBuf::from("/tmp/opt1859-other-source"));
            view.reconcile_selection();
            view.move_selection(-1, false, false, true, cx);
            assert_eq!(view.browser_focus_id(), Some(second));
            view.toggle_random_navigation(cx);
            view.selected = Some(0);
            view.move_selection(1, false, false, true, cx);
            assert_eq!(view.browser_focus_id(), Some(view.entries[1].stable_id()));
        });
    }

    #[gpui::test]
    fn arrow_keys_follow_random_navigation_history(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let first = browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1859-random-keys-fixture");
            view.root = Some(root.clone());
            view.entries = ["a.wav", "b.wav"]
                .into_iter()
                .map(|name| AudioFile {
                    path: root.join(name),
                    relative_path: PathBuf::from(name),
                    byte_len: 0,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                })
                .collect();
            view.view_model = BrowserViewModel::new(&view.entries);
            view.selected = Some(0);
            view.toggle_random_navigation(cx);
            view.entries[0].stable_id()
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let down = KeyDownEvent {
                    keystroke: Keystroke::parse("down").expect("down key"),
                    is_held: false,
                };
                view.handle_key_down(&down, window, cx);
                assert_ne!(view.browser_focus_id(), Some(first.clone()));
                let up = KeyDownEvent {
                    keystroke: Keystroke::parse("up").expect("up key"),
                    is_held: false,
                };
                view.handle_key_down(&up, window, cx);
                assert_eq!(view.browser_focus_id(), Some(first));
            });
        });
    }

    #[gpui::test]
    fn playmark_timing_duration_and_bpm_commit_update_stopped_selection(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-playmark-commit-test.wav");
        let initial = waveform::WaveformPlayRegion::new(0.2, 0.6).expect("play region");
        install_playmark_state(&browser, cx, &path, initial);
        commit_playmark_draft(&browser, cx, "200ms");
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.waveform_play_region,
                Some(waveform::WaveformPlayRegion::new(0.2, 0.4).expect("duration region"))
            );
            assert!(view.playback_init_intent.is_none());
            assert!(view.playing_path.is_none());
            assert!(view.accepted_playback_intent.is_none());
        });

        let bpm_initial = waveform::WaveformPlayRegion::new(0.2, 0.6).expect("BPM region");
        browser.update(cx, |view, _| {
            view.waveform_play_region = Some(bpm_initial);
            view.beat_guides_enabled = false;
            view.beat_guide_count = 4;
        });
        commit_playmark_draft(&browser, cx, "120bpm");
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.waveform_play_region,
                Some(waveform::WaveformPlayRegion::new(0.2, 1.0).expect("BPM region"))
            );
            assert!(view.playback_init_intent.is_none());
            assert!(view.playing_path.is_none());
            assert!(view.accepted_playback_intent.is_none());
        });
    }

    #[gpui::test]
    fn playmark_timing_unchanged_and_equivalent_frames_are_noops(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-playmark-noop-test.wav");
        let initial = waveform::WaveformPlayRegion::new(0.2, 0.4).expect("play region");
        install_playmark_state(&browser, cx, &path, initial);

        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                let original = view.playmark_timing_input.read(cx).content().to_owned();
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content(original, cx));
                view.commit_playmark_timing_edit(window, cx, true);
            });
        });
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.playmark_timing_editor.is_none());
            assert_eq!(view.waveform_play_region, Some(initial));
            assert!(view.playback_init_intent.is_none());
        });

        commit_playmark_draft(&browser, cx, "200ms");
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.playmark_timing_editor.is_none());
            assert_eq!(view.waveform_play_region, Some(initial));
            assert!(view.playback_init_intent.is_none());
        });
    }

    #[gpui::test]
    fn stale_playmark_timing_commit_is_discarded_for_generation_and_selection_changes(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-playmark-stale-test.wav");
        let initial = waveform::WaveformPlayRegion::new(0.2, 0.6).expect("play region");
        install_playmark_state(&browser, cx, &path, initial);

        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content("200ms", cx));
                view.waveform_generation = view.waveform_generation.wrapping_add(1);
                view.commit_playmark_timing_edit(window, cx, true);
            });
        });
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.playmark_timing_editor.is_none());
            assert_eq!(view.waveform_play_region, Some(initial));
        });

        browser.update(cx, |view, _| {
            view.waveform_generation = 7;
            view.selected = Some(0);
            view.waveform_play_region = Some(initial);
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content("200ms", cx));
                view.selected = None;
                view.commit_playmark_timing_edit(window, cx, true);
            });
        });
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.playmark_timing_editor.is_none());
            assert_eq!(view.waveform_play_region, Some(initial));
        });
    }

    #[gpui::test]
    fn playmark_timing_escape_cancels_and_ime_enter_does_not_commit(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-playmark-key-test.wav");
        let initial = waveform::WaveformPlayRegion::new(0.2, 0.6).expect("play region");
        install_playmark_state(&browser, cx, &path, initial);

        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(
                        Some(0..input.content().len()),
                        "200ms",
                        Some(0..5),
                        window,
                        cx,
                    );
                });
                let enter = KeyDownEvent {
                    keystroke: Keystroke::parse("enter").expect("enter keystroke"),
                    is_held: false,
                };
                view.handle_key_down(&enter, window, cx);
                assert!(view.playmark_timing_editor.is_some());
                assert_eq!(view.waveform_play_region, Some(initial));

                let escape = KeyDownEvent {
                    keystroke: Keystroke::parse("escape").expect("escape keystroke"),
                    is_held: false,
                };
                view.handle_key_down(&escape, window, cx);
                assert!(view.playmark_timing_editor.is_none());
                assert_eq!(view.waveform_play_region, Some(initial));
            });
        });
    }

    #[gpui::test]
    fn invalid_playmark_timing_blur_keeps_draft_and_refocuses_editor(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, _| window.activate_window());
        let path = PathBuf::from("/tmp/inert-playmark-timing-test.wav");
        let region = waveform::WaveformPlayRegion::new(0.2, 0.6).expect("play region");
        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/tmp"));
            view.entries = vec![AudioFile {
                path: path.clone(),
                relative_path: PathBuf::from("inert-playmark-timing-test.wav"),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            view.waveform = Some(inert_preview());
            view.waveform_generation = 7;
            view.waveform_play_region = Some(region);
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content("not a timing", cx));
            });
        });
        cx.refresh().expect("refresh active playmark timing editor");
        cx.run_until_parked();
        cx.update(|window, app| {
            let view = browser.read(app);
            assert!(
                view.playmark_timing_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });

        cx.update(|window, app| {
            let view = browser.read(app);
            let search_focus = view.search_input.read(app).focus_handle();
            window.focus(&search_focus);
        });
        cx.refresh().expect("refresh after playmark timing blur");
        cx.run_until_parked();

        cx.update(|window, app| {
            let view = browser.read(app);
            let editor = view
                .playmark_timing_editor
                .as_ref()
                .expect("invalid blur keeps editor open");
            assert_eq!(
                editor.error.as_deref(),
                Some("Use a value such as 1000ms, 1.5s, 1min, or 142bpm")
            );
            assert_eq!(
                view.playmark_timing_input.read(app).content(),
                "not a timing"
            );
            assert!(
                view.playmark_timing_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    fn general_settings_escape_restores_search_focus(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            window.focus(&browser.read(app).search_input.read(app).focus_handle())
        });
        cx.refresh().expect("focused search");
        cx.run_until_parked();
        cx.update(|window, app| {
            assert!(
                browser
                    .read(app)
                    .search_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });
        let bounds = cx.debug_bounds("theme-settings").expect("settings button");
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::none());
        cx.update(|window, app| {
            assert!(
                browser
                    .read(app)
                    .search_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });
        cx.simulate_mouse_up(center, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, app| {
            let view = browser.read(app);
            assert!(view.theme_settings_open);
            assert!(
                view.general_settings
                    .previous_focus
                    .as_ref()
                    .is_some_and(|focus| *focus == view.search_input.read(app).focus_handle())
            );
            assert!(view.focus_handle.is_focused(window));
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, app| {
            let view = browser.read(app);
            assert!(!view.theme_settings_open);
            assert!(
                view.search_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    fn enlarged_settings_controls_wrap_within_narrow_viewport(cx: &mut TestAppContext) {
        let enlarged = crate::theme::with_text_scale(&Theme::default(), 200);
        cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        let window = cx.update(|app| {
            app.open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::new(
                        point(px(0.0), px(0.0)),
                        gpui::size(px(480.0), px(560.0)),
                    ))),
                    ..Default::default()
                },
                |_, app| app.new(Browser::new),
            )
            .expect("open narrow test window")
        });
        let browser = window.root(cx).expect("browser root");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                view.theme_settings_open = true;
                view.general_settings.text_scale_percent = 200;
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, _| assert_eq!(window.bounds().size.width, px(480.0)));
        cx.refresh().expect("enlarged settings layout");
        let strip = cx
            .debug_bounds("theme-settings-strip")
            .expect("settings strip");
        let text_scale = cx
            .debug_bounds("toggle-text-scale")
            .expect("text scale bounds");
        let help_tips = cx
            .debug_bounds("toggle-help-tooltips")
            .expect("help tips bounds");
        assert!(
            help_tips.origin.y > text_scale.origin.y,
            "controls did not wrap: text {text_scale:?}, help {help_tips:?}, strip {strip:?}"
        );

        for selector in ["toggle-text-scale", "toggle-help-tooltips"] {
            let bounds = cx.debug_bounds(selector).expect("settings control bounds");
            assert!(
                bounds.origin.x >= strip.origin.x,
                "{selector} starts outside strip"
            );
            assert!(
                bounds.origin.x + bounds.size.width <= strip.origin.x + strip.size.width,
                "{selector} ends outside strip: {bounds:?}, strip {strip:?}"
            );
            assert!(
                bounds.origin.y + bounds.size.height <= strip.origin.y + strip.size.height,
                "{selector} is below strip: {bounds:?}, strip {strip:?}"
            );
        }
    }

    #[gpui::test]
    fn random_toolbar_command_precedes_shift_without_starting_playback(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| view.waveform = Some(inert_preview()));
        cx.refresh().expect("refresh random toolbar");
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("random-audition-toolbar")
            .expect("random toolbar");
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let modifiers = Modifiers {
            platform: true,
            shift: true,
            ..Modifiers::none()
        };
        cx.simulate_mouse_down(center, MouseButton::Left, modifiers);
        cx.simulate_mouse_up(center, MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.random_audition_sticky);
            assert!(view.random_audition_pending.is_none());
            assert!(view.playback_init_intent.is_none());
            assert!(view.waveform_play_region.is_none());
        });
    }

    #[gpui::test]
    fn root_fade_pointer_routes_both_buttons_without_touching_playback(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-fade-test.wav");
        let play_region = waveform::WaveformPlayRegion::new(0.15, 0.85).expect("play region");
        let edit_region = yield_core::WaveformEditRegion::new(0.2, 0.8).expect("edit region");
        let fade_in = WaveformEditFade::new(0.25, 0.65, 0.2, 0.7).expect("fade in");
        let fade_out = WaveformEditFade::new(0.3, 0.35, 0.15, 0.4).expect("fade out");
        let edit_settings =
            WaveformEditSettings::new(1.4, Some(fade_in), Some(fade_out)).expect("edit settings");
        let accepted = transport::PlaybackIntent {
            start: 0.3,
            region: Some(play_region),
            looped: true,
            volume: 0.8,
            path: path.clone(),
            origin: transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        browser.update(cx, |view, _| {
            view.waveform = Some(inert_preview());
            view.waveform_play_region = Some(play_region);
            view.waveform_edit_region = Some(edit_region);
            view.waveform_edit_settings = edit_settings;
            view.playing_path = Some(path.clone());
            view.accepted_playback_intent = Some(accepted.clone());
            view.playback_epoch = 29;
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let at = |ratio: f64, y: f32| {
            point(
                bounds.origin.x + bounds.size.width * ratio as f32,
                bounds.origin.y + px(y),
            )
        };
        let modifiers = Modifiers::none();

        // The leading inner endpoint is the first standard handle and accepts the primary
        // button. Its drag changes only the fade length.
        let leading_start = at(0.35, 5.0);
        let leading_end = at(0.42, 5.0);
        cx.simulate_mouse_down(leading_start, MouseButton::Left, modifiers);
        cx.simulate_mouse_move(leading_end, MouseButton::Left, modifiers);
        cx.simulate_mouse_up(leading_end, MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let fade = view
                .waveform_edit_settings
                .fade_in()
                .expect("leading fade remains present");
            assert!((fade.length() - (0.42_f32 - 0.2) / 0.6).abs() < 1.0e-5);
            assert_eq!(fade.curve(), fade_in.curve());
            assert_eq!(fade.mute(), fade_in.mute());
            assert_eq!(fade.outer_gain(), fade_in.outer_gain());
            assert_eq!(view.waveform_edit_settings.fade_out(), Some(fade_out));
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 29);
        });

        // The trailing inner endpoint accepts the secondary button through the same shared drag
        // owner, while keeping the edit selection and playback state independent.
        let trailing_start = at(0.62, 5.0);
        let trailing_end = at(0.58, 5.0);
        cx.simulate_mouse_down(trailing_start, MouseButton::Right, modifiers);
        cx.simulate_mouse_move(trailing_end, MouseButton::Right, modifiers);
        cx.simulate_mouse_up(trailing_end, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let fade = view
                .waveform_edit_settings
                .fade_out()
                .expect("trailing fade remains present");
            assert!((fade.length() - (0.8_f32 - 0.58) / 0.6).abs() < 1.0e-5);
            assert_eq!(fade.curve(), fade_out.curve());
            assert_eq!(fade.mute(), fade_out.mute());
            assert_eq!(fade.outer_gain(), fade_out.outer_gain());
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 29);
        });
    }

    #[gpui::test]
    fn root_fade_button_loss_cancels_without_committing_effects(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-fade-cancel-test.wav");
        let play_region = waveform::WaveformPlayRegion::new(0.1, 0.9).expect("play region");
        let edit_region = yield_core::WaveformEditRegion::new(0.2, 0.8).expect("edit region");
        let fade_in = WaveformEditFade::new(0.25, 0.65, 0.2, 0.7).expect("fade in");
        let fade_out = WaveformEditFade::new(0.3, 0.35, 0.15, 0.4).expect("fade out");
        let edit_settings =
            WaveformEditSettings::new(1.4, Some(fade_in), Some(fade_out)).expect("edit settings");
        let accepted = transport::PlaybackIntent {
            start: 0.3,
            region: Some(play_region),
            looped: true,
            volume: 0.8,
            path: path.clone(),
            origin: transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        browser.update(cx, |view, _| {
            view.waveform = Some(inert_preview());
            view.waveform_play_region = Some(play_region);
            view.waveform_edit_region = Some(edit_region);
            view.waveform_edit_settings = edit_settings;
            view.playing_path = Some(path.clone());
            view.accepted_playback_intent = Some(accepted.clone());
            view.playback_epoch = 31;
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let at = |ratio: f64| {
            point(
                bounds.origin.x + bounds.size.width * ratio as f32,
                bounds.origin.y + px(5.0),
            )
        };
        let modifiers = Modifiers::none();
        let start = at(0.35);
        let moved = at(0.48);
        cx.simulate_mouse_down(start, MouseButton::Left, modifiers);
        cx.simulate_mouse_move(moved, MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(
                view.waveform_region_drag
                    .is_some_and(|drag| drag.is_edit_fade())
            );
            assert_eq!(view.waveform_edit_settings, edit_settings);
        });

        // Losing the button cancels the shared drag owner before its preview can become a commit.
        cx.simulate_mouse_move(moved, None, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_edit_settings, edit_settings);
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 31);
        });
    }

    #[gpui::test]
    fn root_fade_double_click_clears_only_standard_outer_mute(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let edit_region = yield_core::WaveformEditRegion::new(0.35, 0.75).expect("edit region");
        let fade_in = WaveformEditFade::new(0.25, 0.65, 0.2, 1.0).expect("fade in");
        let settings = WaveformEditSettings::new(1.4, Some(fade_in), None).expect("edit settings");
        browser.update(cx, |view, _| {
            view.waveform = Some(inert_preview());
            view.waveform_edit_region = Some(edit_region);
            view.waveform_edit_settings = settings;
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let outer_ratio = edit_region.start() - edit_region.width() * f64::from(fade_in.mute());
        let standard_outer = point(
            bounds.origin.x + bounds.size.width * outer_ratio as f32,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let modifiers = Modifiers::none();
        cx.simulate_event(MouseDownEvent {
            position: standard_outer,
            modifiers,
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_mouse_up(standard_outer, MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let cleared = view
                .waveform_edit_settings
                .fade_in()
                .expect("cleared fade remains present");
            assert_eq!(cleared.length(), fade_in.length());
            assert_eq!(cleared.curve(), fade_in.curve());
            assert_eq!(cleared.mute(), 0.0);
            assert_eq!(cleared.outer_gain(), fade_in.outer_gain());
            assert_eq!(view.waveform_edit_settings.gain(), settings.gain());
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert!(view.waveform_region_drag.is_none());
        });

        // Restore a muted fade whose vertical outer-gain handle overlaps the centerline handle.
        // The larger vertical target has priority, and its double click must leave mute intact.
        let outer_gain_settings = WaveformEditSettings::new(
            settings.gain(),
            Some(
                WaveformEditFade::new(fade_in.length(), fade_in.curve(), fade_in.mute(), 0.0)
                    .expect("outer-gain test fade"),
            ),
            None,
        )
        .expect("outer-gain settings");
        browser.update(cx, |view, _| {
            view.waveform_edit_settings = outer_gain_settings;
        });
        cx.refresh().expect("refresh restored fade");
        cx.run_until_parked();
        let outer_gain = point(
            bounds.origin.x + bounds.size.width * outer_ratio as f32,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_event(MouseDownEvent {
            position: outer_gain,
            modifiers,
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_mouse_up(outer_gain, MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.waveform_edit_settings
                    .fade_in()
                    .expect("vertical handle fade")
                    .mute(),
                fade_in.mute()
            );
            assert!(view.waveform_region_drag.is_none());
        });
    }

    #[gpui::test]
    fn root_right_pointer_routes_edit_selection_without_touching_playback(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        assert!(!cx
            .cx
            .read(|app| app.has_global::<super::super::source_registry::SourceRegistryGlobal>()));
        let path = PathBuf::from("/tmp/inert-waveform-test.wav");
        let play_region = waveform::WaveformPlayRegion::new(0.25, 0.75).expect("play region");
        let accepted = transport::PlaybackIntent {
            start: 0.25,
            region: Some(play_region),
            looped: true,
            volume: 0.8,
            path: path.clone(),
            origin: transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        browser.update(cx, |view, _| {
            assert!(view.source_registry_task.is_none());
            assert!(view.preferences_task.is_none());
            assert!(view.playback.is_none());
            view.waveform = Some(inert_preview());
            view.waveform_play_region = Some(play_region);
            view.playing_path = Some(path.clone());
            view.accepted_playback_intent = Some(accepted.clone());
            view.playback_epoch = 19;
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let start = point(
            bounds.origin.x + bounds.size.width * 0.1,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let end = point(
            bounds.origin.x + bounds.size.width * 0.35,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let outside_waveform = point(
            bounds.origin.x + bounds.size.width + px(24.0),
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let modifiers = Modifiers::none();

        cx.simulate_mouse_down(start, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.waveform_region_drag.map(|drag| drag.selection_kind()),
                Some(waveform::WaveformSelectionKind::Edit)
            );
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });

        cx.simulate_mouse_move(end, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_some());
            assert!(view.waveform_edit_region.is_none());
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });

        cx.simulate_mouse_up(end, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_some_and(|region| {
                (region.start() - 0.1).abs() < 1.0e-6 && (region.end() - 0.35).abs() < 1.0e-6
            }));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });

        cx.simulate_mouse_down(
            point(
                bounds.origin.x + bounds.size.width * 0.45,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            MouseButton::Right,
            modifiers,
        );
        cx.simulate_mouse_up(
            point(
                bounds.origin.x + bounds.size.width * 0.45,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            MouseButton::Right,
            modifiers,
        );
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_none());
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });

        cx.simulate_mouse_down(
            point(
                bounds.origin.x + bounds.size.width * 0.4,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            MouseButton::Right,
            modifiers,
        );
        cx.simulate_mouse_move(
            point(
                bounds.origin.x + bounds.size.width * 0.6,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            MouseButton::Right,
            modifiers,
        );
        cx.simulate_mouse_up(outside_waveform, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_some_and(|region| {
                (region.start() - 0.4).abs() < 1.0e-6 && (region.end() - 1.0).abs() < 1.0e-6
            }));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });

        cx.simulate_mouse_down(
            point(
                bounds.origin.x + bounds.size.width * 0.2,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            MouseButton::Right,
            modifiers,
        );
        cx.simulate_mouse_move(
            point(
                bounds.origin.x + bounds.size.width * 0.6,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            None,
            modifiers,
        );
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_some());
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });
        cx.simulate_mouse_up(outside_waveform, MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_some());
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 19);
        });
    }

    #[gpui::test]
    fn root_gain_pointer_routes_both_buttons_without_touching_playback(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/inert-gain-test.wav");
        let play_region = waveform::WaveformPlayRegion::new(0.2, 0.8).expect("play region");
        let edit_region = yield_core::WaveformEditRegion::new(0.25, 0.75).expect("edit region");
        let accepted = transport::PlaybackIntent {
            start: 0.2,
            region: Some(play_region),
            looped: false,
            volume: 0.7,
            path: path.clone(),
            origin: transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        browser.update(cx, |view, _| {
            view.waveform = Some(inert_preview());
            view.waveform_play_region = Some(play_region);
            view.waveform_edit_region = Some(edit_region);
            view.waveform_edit_settings = waveform::default_edit_settings();
            view.playing_path = Some(path.clone());
            view.accepted_playback_intent = Some(accepted.clone());
            view.playback_epoch = 23;
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();

        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let center_x = bounds.origin.x + bounds.size.width * 0.5;
        let handle_y = bounds.origin.y + px(8.0);
        let boosted_y = bounds.origin.y - px(32.0);
        let attenuated_y = bounds.origin.y + px(48.0);
        let modifiers = Modifiers::none();

        cx.simulate_mouse_down(point(center_x, handle_y), MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let drag = view.waveform_region_drag.expect("left gain drag");
            assert!(drag.is_edit_gain());
            assert_eq!(drag.selection_kind(), waveform::WaveformSelectionKind::Edit);
            assert_eq!(drag.button(), MouseButton::Left);
            assert_eq!(view.waveform_edit_settings.gain(), 1.0);
        });
        // Cross GPUI's drag threshold while still over the waveform before moving above its
        // bounds. Once the drag is active, the capture-phase drag listener follows the pointer
        // outside the element as the native handle does.
        cx.simulate_mouse_move(
            point(center_x, bounds.origin.y + px(4.0)),
            MouseButton::Left,
            modifiers,
        );
        cx.simulate_mouse_move(point(center_x, boosted_y), MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let drag = view.waveform_region_drag.expect("left gain preview");
            assert!((drag.preview_edit_settings().gain() - 2.0).abs() < 1.0e-6);
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 23);
        });
        cx.simulate_mouse_up(point(center_x, boosted_y), MouseButton::Left, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!((view.waveform_edit_settings.gain() - 2.0).abs() < 1.0e-6);
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 23);
        });

        cx.simulate_mouse_down(point(center_x, handle_y), MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            let drag = view.waveform_region_drag.expect("right gain drag");
            assert!(drag.is_edit_gain());
            assert_eq!(drag.button(), MouseButton::Right);
        });
        cx.simulate_mouse_move(point(center_x, attenuated_y), MouseButton::Right, modifiers);
        cx.simulate_mouse_up(point(center_x, attenuated_y), MouseButton::Right, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert!((view.waveform_edit_settings.gain() - (2.0 - 40.0 / 120.0)).abs() < 1.0e-6);
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 23);
        });

        let committed_gain = cx
            .cx
            .read(|app| browser.read(app).waveform_edit_settings.gain());
        cx.simulate_mouse_down(point(center_x, handle_y), MouseButton::Left, modifiers);
        cx.simulate_mouse_move(
            point(center_x, bounds.origin.y + px(4.0)),
            MouseButton::Left,
            modifiers,
        );
        cx.simulate_mouse_move(point(center_x, boosted_y), MouseButton::Left, modifiers);
        cx.simulate_mouse_move(point(center_x, boosted_y), None, modifiers);
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.waveform_region_drag.is_none());
            assert_eq!(view.waveform_edit_settings.gain(), committed_gain);
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 23);
        });
    }

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
