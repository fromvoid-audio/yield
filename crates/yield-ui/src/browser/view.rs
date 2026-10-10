#![deny(missing_docs)]
#![deny(warnings)]

//! Browser rendering and the compact Yield application shell.

use gpui::{
    AnyElement, ClickEvent, Context, Div, DragMoveEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, StatefulInteractiveElement, Window,
    WindowControlArea, canvas, div, font, prelude::*, px, uniform_list,
};
use std::time::{SystemTime, UNIX_EPOCH};
use yield_core::{PlaybackType, SortColumn, SortDirection, format_duration, format_last_played};
use yield_design::Theme;

use crate::theme::{active, color, transparent};

use super::Browser;
use super::column_layout::{BrowserColumn, ColumnGesture, DRAG_THRESHOLD, ResolvedColumn};
use super::context_menu;
use super::curation;
use super::display_labels;
use super::feedback_issue;
use super::file_drag;
use super::metronome;
use super::motion;
use super::normalized_audition;
use super::panel_resize::{ResizeDrag, bounded_size};
use super::random_audition;
use super::shortcut_help;
use super::similar_sections;
use super::tag_filter;
use super::volume;
use super::waveform;
use super::zero_crossing;

struct RandomNavigationTooltip;

struct FocusLoadedTooltip;

impl Render for FocusLoadedTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_raised))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_primary))
            .child("Focus the loaded sample in the browser")
    }
}

fn focus_loaded_tooltip(_: &mut Window, cx: &mut gpui::App) -> gpui::AnyView {
    cx.new(|_| FocusLoadedTooltip).into()
}

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

fn browser_text_cell(text: String, width: f32, theme: &Theme) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .px(px(theme.spacing.control_gap))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_muted))
        .child(text)
}

fn current_unix_epoch_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

const MIN_SAMPLE_NAME_WIDTH: f32 = 192.0;
const MIN_SIDEBAR_WIDTH: f32 = 180.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;
const MIN_WAVEFORM_HEIGHT: f32 = 94.0;
const MAX_WAVEFORM_HEIGHT: f32 = 512.0;
const MIN_MAIN_PANE_WIDTH: f32 = 240.0;
const MIN_LIST_ROWS: f32 = 4.0;
const PANE_RESIZE_RAIL: f32 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PaneAxis {
    Sidebar,
    Filter,
    Waveform,
    Metadata,
    Collections,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PaneResizeDrag {
    axis: PaneAxis,
    gesture: ResizeDrag,
    viewport: gpui::Size<Pixels>,
    row_height: f32,
}

#[derive(Clone, Copy, Debug)]
struct SidebarResizeToken;

#[derive(Clone, Copy, Debug)]
struct WaveformResizeToken;

#[derive(Clone, Copy, Debug)]
struct CollectionsResizeToken;

struct PaneResizeGhost;

impl Render for PaneResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn sidebar_width(preferred: Option<f32>, window_width: f32, theme: &Theme) -> f32 {
    let maximum = (window_width - MIN_MAIN_PANE_WIDTH - PANE_RESIZE_RAIL)
        .max(96.0)
        .min(MAX_SIDEBAR_WIDTH);
    bounded_size(
        preferred.unwrap_or(theme.spacing.sidebar_width),
        MIN_SIDEBAR_WIDTH.min(maximum),
        maximum,
    )
}

pub(super) fn collections_height(preferred: Option<f32>, window_height: f32, theme: &Theme) -> f32 {
    let minimum = theme.spacing.row_height * 2.0 + PANE_RESIZE_RAIL;
    let maximum = (window_height * 0.25)
        .max(minimum)
        .min(360.0_f32.max(minimum));
    bounded_size(
        preferred.unwrap_or(theme.spacing.row_height * 5.0),
        minimum,
        maximum,
    )
}

fn browser_toolbar_gap(window_width: f32, theme: &Theme) -> f32 {
    if window_width < 1_120.0 {
        theme.spacing.control_gap.min(4.0)
    } else {
        theme.spacing.control_gap
    }
}

fn browser_toolbar_height(window_width: f32, theme: &Theme, enlarged: bool) -> f32 {
    if !enlarged {
        return theme.spacing.toolbar_height;
    }
    let rows = 2.0;
    theme.spacing.control_height * rows
        + browser_toolbar_gap(window_width, theme) * (rows - 1.0)
        + theme.spacing.panel_padding * 2.0
}

fn browser_status_bar_height(theme: &Theme, enlarged: bool) -> f32 {
    theme.spacing.status_height * if enlarged { 2.0 } else { 1.0 }
}

fn waveform_height(
    preferred: Option<f32>,
    window_height: f32,
    theme: &Theme,
    toolbar_height: f32,
    status_height: f32,
) -> f32 {
    let minimum = MIN_WAVEFORM_HEIGHT.max(
        theme.waveform.title_height
            + theme.waveform.metadata_height
            + theme.waveform.scrollbar_height
            + theme.spacing.waveform_min_bar_height,
    );
    let maximum = (window_height
        - toolbar_height
        - status_height
        - theme.spacing.row_height * MIN_LIST_ROWS
        - PANE_RESIZE_RAIL)
        .max(minimum)
        .min(MAX_WAVEFORM_HEIGHT.max(minimum));
    bounded_size(
        preferred.unwrap_or(theme.spacing.waveform_height),
        minimum,
        maximum,
    )
}

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

fn browser_column_width_scale(text_scale_percent: u16) -> f32 {
    if text_scale_percent == yield_session::ENLARGED_TEXT_SCALE_PERCENT {
        2.0
    } else {
        1.0
    }
}

impl Browser {
    fn begin_column_reorder(&mut self, source: BrowserColumn, x: f32) {
        self.suppress_column_sort_click = false;
        self.column_gesture = Some(ColumnGesture::Reorder {
            source,
            start_x: x,
            insertion: None,
        });
    }

    fn begin_column_resize(
        &mut self,
        source: BrowserColumn,
        x: f32,
        start_width: f32,
        name_width: f32,
        window: &mut Window,
    ) {
        window.focus(&self.focus_handle);
        self.suppress_column_sort_click = true;
        let previous_override = self.column_layout.width_override(source);
        let previous_name_override = self.column_layout.width_override(BrowserColumn::Name);
        let width_scale = browser_column_width_scale(self.general_settings.text_scale_percent);
        if source != BrowserColumn::Name {
            self.column_layout
                .freeze_name_width(name_width / width_scale);
        }
        self.column_gesture = Some(ColumnGesture::Resize {
            source,
            start_x: x,
            start_width,
            previous_override,
            previous_name_override,
        });
    }

    fn update_column_gesture(
        &mut self,
        x: f32,
        pressed: Option<MouseButton>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if pressed != Some(MouseButton::Left) {
            return;
        }
        let Some(gesture) = self.column_gesture else {
            return;
        };
        match gesture {
            ColumnGesture::Resize {
                source,
                start_x,
                start_width,
                ..
            } => {
                let width_scale =
                    browser_column_width_scale(self.general_settings.text_scale_percent);
                if self
                    .column_layout
                    .set_width(source, (start_width + x - start_x) / width_scale)
                {
                    cx.notify();
                }
            }
            ColumnGesture::Reorder {
                source,
                start_x,
                insertion,
            } => {
                if insertion.is_none() && (x - start_x).abs() < DRAG_THRESHOLD {
                    return;
                }
                let Some(bounds) = *self.column_header_bounds.borrow() else {
                    return;
                };
                let theme = active(cx);
                let visibility =
                    browser_column_visibility(f32::from(window.bounds().size.width), &theme);
                let width_scale =
                    browser_column_width_scale(self.general_settings.text_scale_percent);
                let columns = self.column_layout.resolve(
                    &theme,
                    visibility,
                    f32::from(bounds.size.width),
                    width_scale,
                );
                let mut left = f32::from(bounds.origin.x) + theme.spacing.selected_marker_width;
                let mut target = 0;
                for column in &columns {
                    if column.id != source {
                        if x < left + column.width / 2.0 {
                            break;
                        }
                        target += 1;
                    }
                    left += column.width;
                }
                if insertion != Some(target) {
                    if insertion.is_none() {
                        window.focus(&self.focus_handle);
                    }
                    self.column_gesture = Some(ColumnGesture::Reorder {
                        source,
                        start_x,
                        insertion: Some(target),
                    });
                    self.suppress_column_sort_click = true;
                    cx.notify();
                }
            }
        }
    }

    fn finish_column_gesture(&mut self, commit: bool, window: &Window, cx: &mut Context<Self>) {
        let Some(gesture) = self.column_gesture.take() else {
            return;
        };
        if let ColumnGesture::Reorder {
            source,
            insertion: Some(insertion),
            ..
        } = gesture
        {
            if commit {
                let theme = active(cx);
                let visibility =
                    browser_column_visibility(f32::from(window.bounds().size.width), &theme);
                self.column_layout
                    .reorder_visible(source, insertion, visibility);
            }
        } else if let ColumnGesture::Resize {
            source,
            previous_override,
            previous_name_override,
            ..
        } = gesture
        {
            if !commit {
                self.column_layout
                    .restore_width_override(source, previous_override);
                self.column_layout
                    .restore_width_override(BrowserColumn::Name, previous_name_override);
            }
        }
        cx.notify();
    }

    pub(super) fn cancel_column_gesture(&mut self, cx: &mut Context<Self>) {
        if let Some(ColumnGesture::Resize {
            source,
            previous_override,
            previous_name_override,
            ..
        }) = self.column_gesture.take()
        {
            self.column_layout
                .restore_width_override(source, previous_override);
            self.column_layout
                .restore_width_override(BrowserColumn::Name, previous_name_override);
        }
        cx.notify();
    }

    fn render_browser_header_column(
        &self,
        column: ResolvedColumn,
        name_width: f32,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content: AnyElement = if column.id == BrowserColumn::Name {
            div()
                .flex()
                .flex_none()
                .w(px(column.width))
                .child(self.render_column_header(
                    "Name",
                    SortColumn::Name,
                    column.id,
                    theme.clone(),
                    cx,
                    None,
                    theme.spacing.row_padding_x,
                ))
                .child(self.render_random_navigation_button(theme.clone(), cx))
                .child(self.render_sample_name_view_mode_button(theme.clone(), cx))
                .into_any_element()
        } else if let Some(sort) = column.id.sort_column() {
            self.render_column_header(
                column.id.label(),
                sort,
                column.id,
                theme.clone(),
                cx,
                Some(column.width),
                theme.spacing.control_gap,
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .flex_none()
                .w(px(column.width))
                .h(px(theme.spacing.row_height))
                .pl(px(theme.spacing.control_gap))
                .pr(px(theme.spacing.control_gap))
                .border_b(px(theme.spacing.border_width))
                .border_color(color(theme.colors.border))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child(column.id.label())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, event: &MouseDownEvent, _, _| {
                        view.begin_column_reorder(column.id, f32::from(event.position.x));
                    }),
                )
                .into_any_element()
        };
        div()
            .relative()
            .debug_selector(move || format!("browser-column-{:?}", column.id))
            .flex_none()
            .w(px(column.width))
            .h(px(theme.spacing.row_height))
            .child(content)
            .child(
                div()
                    .absolute()
                    .debug_selector(move || format!("browser-column-resize-{:?}", column.id))
                    .right_0()
                    .top_0()
                    .h_full()
                    .w(px(8.0))
                    .cursor_col_resize()
                    .hover(move |style| style.bg(color(theme.colors.border_emphasis)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                            view.begin_column_resize(
                                column.id,
                                f32::from(event.position.x),
                                column.width,
                                name_width,
                                window,
                            );
                            cx.stop_propagation();
                        }),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn begin_pane_resize(
        &mut self,
        axis: PaneAxis,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pane_resize(cx);
        let theme = active(cx);
        let (start_size, previous_preference) = match axis {
            PaneAxis::Metadata => (
                super::metadata_panel::height(
                    self.preferred_metadata_height,
                    f32::from(window.bounds().size.height),
                    theme,
                ),
                self.preferred_metadata_height,
            ),
            PaneAxis::Sidebar => (
                sidebar_width(
                    self.preferred_sidebar_width,
                    f32::from(window.bounds().size.width),
                    theme,
                ),
                self.preferred_sidebar_width,
            ),
            PaneAxis::Collections => (
                collections_height(
                    self.preferred_collections_height,
                    f32::from(window.bounds().size.height),
                    theme,
                ),
                self.preferred_collections_height,
            ),
            PaneAxis::Filter => (
                super::filter_panel::height(
                    self.preferred_filter_height,
                    f32::from(window.viewport_size().height),
                    theme,
                ),
                self.preferred_filter_height,
            ),
            PaneAxis::Waveform => (
                waveform_height(
                    self.preferred_waveform_height,
                    f32::from(window.bounds().size.height),
                    theme,
                    browser_toolbar_height(
                        f32::from(window.bounds().size.width),
                        theme,
                        self.general_settings.text_scale_percent
                            == yield_session::ENLARGED_TEXT_SCALE_PERCENT,
                    ),
                    browser_status_bar_height(
                        theme,
                        self.general_settings.text_scale_percent
                            == yield_session::ENLARGED_TEXT_SCALE_PERCENT,
                    ),
                ),
                self.preferred_waveform_height,
            ),
        };
        self.pane_resize_drag = Some(PaneResizeDrag {
            axis,
            gesture: ResizeDrag::begin(
                match axis {
                    PaneAxis::Sidebar => f32::from(position.x),
                    PaneAxis::Filter
                    | PaneAxis::Waveform
                    | PaneAxis::Metadata
                    | PaneAxis::Collections => f32::from(position.y),
                },
                start_size,
                previous_preference,
            ),
            viewport: window.bounds().size,
            row_height: theme.spacing.row_height,
        });
    }

    pub(super) fn update_pane_resize(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.pane_resize_drag else {
            return;
        };
        if !self.pane_resize_context_matches(window, cx) {
            self.cancel_pane_resize(cx);
            return;
        }
        let theme = active(cx);
        match drag.axis {
            PaneAxis::Metadata => {
                self.preferred_metadata_height = Some(super::metadata_panel::height(
                    Some(drag.gesture.requested_size(f32::from(position.y))),
                    f32::from(window.bounds().size.height),
                    theme,
                ));
            }
            PaneAxis::Sidebar => {
                let requested = drag.gesture.requested_size(f32::from(position.x));
                self.preferred_sidebar_width = Some(sidebar_width(
                    Some(requested),
                    f32::from(window.bounds().size.width),
                    theme,
                ));
            }
            PaneAxis::Collections => {
                let requested = drag.gesture.requested_size(f32::from(position.y));
                self.preferred_collections_height = Some(collections_height(
                    Some(requested),
                    f32::from(window.bounds().size.height),
                    theme,
                ));
            }
            PaneAxis::Filter => {
                let requested = drag.gesture.requested_size(f32::from(position.y));
                self.preferred_filter_height = Some(super::filter_panel::height(
                    Some(requested),
                    f32::from(window.viewport_size().height),
                    theme,
                ));
            }
            PaneAxis::Waveform => {
                let requested = drag.gesture.requested_size(f32::from(position.y));
                self.preferred_waveform_height = Some(waveform_height(
                    Some(requested),
                    f32::from(window.bounds().size.height),
                    theme,
                    browser_toolbar_height(
                        f32::from(window.bounds().size.width),
                        theme,
                        self.general_settings.text_scale_percent
                            == yield_session::ENLARGED_TEXT_SCALE_PERCENT,
                    ),
                    browser_status_bar_height(
                        theme,
                        self.general_settings.text_scale_percent
                            == yield_session::ENLARGED_TEXT_SCALE_PERCENT,
                    ),
                ));
            }
        }
        cx.notify();
    }

    pub(super) fn finish_pane_resize(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.update_pane_resize(position, window, cx);
        self.pane_resize_drag = None;
    }

    pub(super) fn cancel_pane_resize(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.pane_resize_drag.take() else {
            return;
        };
        match drag.axis {
            PaneAxis::Metadata => {
                self.preferred_metadata_height = drag.gesture.previous_preference()
            }
            PaneAxis::Sidebar => self.preferred_sidebar_width = drag.gesture.previous_preference(),
            PaneAxis::Filter => self.preferred_filter_height = drag.gesture.previous_preference(),
            PaneAxis::Collections => {
                self.preferred_collections_height = drag.gesture.previous_preference()
            }
            PaneAxis::Waveform => {
                self.preferred_waveform_height = drag.gesture.previous_preference()
            }
        }
        cx.notify();
    }

    fn pane_resize_context_matches(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.pane_resize_drag.is_none_or(|drag| {
            drag.viewport == window.bounds().size
                && drag.row_height == active(cx).spacing.row_height
        })
    }

    fn reconcile_pane_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_resize_environment_subscriptions.is_empty() {
            let focus = self.focus_handle.clone();
            self.pane_resize_environment_subscriptions = vec![
                cx.observe_window_bounds(window, |view, _, cx| view.cancel_pane_resize(cx)),
                cx.on_focus_out(&focus, window, |view, _, _, cx| view.cancel_pane_resize(cx)),
            ];
        }
        if self.pane_resize_activation.is_none() {
            self.pane_resize_activation =
                Some(cx.observe_window_activation(window, |view, window, cx| {
                    if !window.is_window_active() {
                        view.cancel_pane_resize(cx);
                    }
                }));
        }
        if !self.pane_resize_context_matches(window, cx) {
            self.cancel_pane_resize(cx);
        }
    }

    fn render_sidebar_resize_rail(&self, theme: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let rail_color = color(theme.colors.border);
        let hover_color = color(theme.colors.border_emphasis);
        div()
            .id("workspace-sidebar-resize")
            .debug_selector(|| String::from("workspace-sidebar-resize"))
            .flex_none()
            .w(px(PANE_RESIZE_RAIL))
            .h_full()
            .cursor_col_resize()
            .bg(rail_color)
            .hover(move |style| style.bg(hover_color))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.prevent_default();
                    view.begin_pane_resize(PaneAxis::Sidebar, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_drag(SidebarResizeToken, |_, _, _, cx| {
                cx.new(|_| PaneResizeGhost)
            })
            .on_drag_move::<SidebarResizeToken>(cx.listener(
                |view, event: &DragMoveEvent<SidebarResizeToken>, window, cx| {
                    view.update_pane_resize(event.event.position, window, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
    }

    pub(super) fn render_collections_resize_rail(
        &self,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rail_color = color(theme.colors.border);
        let hover_color = color(theme.colors.border_emphasis);
        div()
            .id("sidebar-collections-resize")
            .debug_selector(|| String::from("sidebar-collections-resize"))
            .flex_none()
            .w_full()
            .h(px(PANE_RESIZE_RAIL))
            .cursor_row_resize()
            .bg(rail_color)
            .hover(move |style| style.bg(hover_color))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.prevent_default();
                    view.begin_pane_resize(PaneAxis::Collections, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_drag(CollectionsResizeToken, |_, _, _, cx| {
                cx.new(|_| PaneResizeGhost)
            })
            .on_drag_move::<CollectionsResizeToken>(cx.listener(
                |view, event: &DragMoveEvent<CollectionsResizeToken>, window, cx| {
                    view.update_pane_resize(event.event.position, window, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
    }

    fn render_waveform_resize_rail(
        &self,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rail_color = color(theme.colors.border);
        let hover_color = color(theme.colors.border_emphasis);
        div()
            .id("workspace-waveform-resize")
            .debug_selector(|| String::from("workspace-waveform-resize"))
            .flex_none()
            .w_full()
            .h(px(PANE_RESIZE_RAIL))
            .cursor_row_resize()
            .bg(rail_color)
            .hover(move |style| style.bg(hover_color))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.prevent_default();
                    view.begin_pane_resize(PaneAxis::Waveform, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_drag(WaveformResizeToken, |_, _, _, cx| {
                cx.new(|_| PaneResizeGhost)
            })
            .on_drag_move::<WaveformResizeToken>(cx.listener(
                |view, event: &DragMoveEvent<WaveformResizeToken>, window, cx| {
                    view.update_pane_resize(event.event.position, window, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_pane_resize(event.position, window, cx);
                }),
            )
    }

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
            let space_action = if self.random_audition_sticky {
                "Space auditions a random section"
            } else {
                "Space replays"
            };
            (
                format!(
                    "Preview loaded: {}  ·  {space_action}  ·  Esc stops",
                    format_duration(Some(preview.duration_seconds)),
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
        browser_column: BrowserColumn,
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
        header
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, _, _| {
                    view.begin_column_reorder(browser_column, f32::from(event.position.x));
                }),
            )
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if view.suppress_column_sort_click {
                    view.suppress_column_sort_click = false;
                } else {
                    view.toggle_sort(column, cx);
                }
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
        super::harvest_filter::invalidate_if_stale(self, cx);
        self.reconcile_pane_resize(window, cx);
        self.reconcile_shipped_visual_effect(window, cx);
        self.refresh_user_shader_review_token();
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
        super::similarity_list::invalidate_if_stale(self);
        let mut theme = active(cx).clone();
        let enlarged_text =
            self.general_settings.text_scale_percent == yield_session::ENLARGED_TEXT_SCALE_PERCENT;
        theme.spacing.sidebar_width = sidebar_width(
            self.preferred_sidebar_width,
            f32::from(window.bounds().size.width),
            &theme,
        );
        theme.spacing.waveform_height = waveform_height(
            self.preferred_waveform_height,
            f32::from(window.bounds().size.height),
            &theme,
            browser_toolbar_height(f32::from(window.bounds().size.width), &theme, enlarged_text),
            browser_status_bar_height(&theme, enlarged_text),
        );
        let motion_frame = self.motion_frame(theme.motion, window);
        let jobs = super::job_details::snapshot(self, cx);
        let jobs_status = jobs.status_label();
        let jobs_running = jobs.active_count() > 0;
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
        } else if self.view_model.harvest_filter().is_pending() {
            super::harvest_filter::status_label(self)
                .unwrap_or_else(|| "Loading Harvest filter…".into())
        } else if self.view_model.curation_queue_pending() {
            super::curation_queue::status_label(self)
                .unwrap_or_else(|| "Loading Curation queue…".to_string())
        } else if !self.entries.is_empty() && self.view_model.rows().len() != self.entries.len() {
            format!(
                "{} of {} audio files visible",
                self.view_model.rows().len(),
                self.entries.len()
            )
        } else {
            self.scan_status.clone()
        };
        self.observe_statuses(Some(&list_status));
        let curation_status = self.active_curation_status();
        let selected_count = self.selected_count_label();
        let metadata_status = self.catalog_metadata.status_label();
        let display_labels_status = self.display_labels.status_label();
        let zero_crossing_status = self.zero_crossing.status_label();
        let now_epoch_seconds = current_unix_epoch_seconds();
        let column_visibility =
            browser_column_visibility(f32::from(window.bounds().size.width), &theme);
        let available_table_width = (f32::from(window.bounds().size.width)
            - theme.spacing.sidebar_width
            - theme.spacing.selected_marker_width)
            .max(MIN_SAMPLE_NAME_WIDTH);
        let resolved_columns = self.column_layout.resolve(
            &theme,
            column_visibility,
            available_table_width,
            browser_column_width_scale(self.general_settings.text_scale_percent),
        );
        let name_column_width = resolved_columns
            .iter()
            .find(|column| column.id == BrowserColumn::Name)
            .map_or(MIN_SAMPLE_NAME_WIDTH, |column| column.width);
        let table_width = theme.spacing.selected_marker_width
            + resolved_columns
                .iter()
                .map(|column| column.width)
                .sum::<f32>();
        let row_columns = resolved_columns.clone();
        let toolbar_hover = theme.interaction.hover_fill;
        let toolbar_pressed = theme.interaction.pressed_fill;
        let toolbar_gap = browser_toolbar_gap(f32::from(window.bounds().size.width), &theme);
        let toolbar_height =
            browser_toolbar_height(f32::from(window.bounds().size.width), &theme, enlarged_text);
        let status_bar_height = browser_status_bar_height(&theme, enlarged_text);
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
        let transient_markers_color = color(if self.transient_markers.enabled() {
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
                                            .w(px(table_width))
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
                                            );
                                        let mut sample_row = sample_row;
                                        for column in &row_columns {
                                            let cell: AnyElement = match column.id {
                                                BrowserColumn::Name => div()
                                                    .flex()
                                                    .items_center()
                                                    .debug_selector(move || format!("browser-row-name-{visible_index}"))
                                                    .flex_none()
                                                    .w(px(column.width))
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
                                                    .child(super::similarity_list::anchor_button(view, &entry.relative_path, index, cx))
                                                    .child(div().debug_selector(move || format!("browser-row-label-{visible_index}")).overflow_hidden().text_ellipsis().child(sample_name.clone()))
                                                    .into_any_element(),
                                                BrowserColumn::Rating => curation::rating_cell(
                                                    &entry, &list_theme, column.width,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::PlaybackType => browser_text_cell(
                                                    playback_type_label(entry.playback_type).into(),
                                                    column.width,
                                                    &list_theme,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::Collection => curation::collection_cell(
                                                    &entry, &list_theme, column.width,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::Extension => browser_text_cell(
                                                    extension.clone(), column.width, &list_theme,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::Size => browser_text_cell(
                                                    size.clone(), column.width, &list_theme,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::Duration => browser_text_cell(
                                                    duration.clone(), column.width, &list_theme,
                                                )
                                                .into_any_element(),
                                                BrowserColumn::LastPlayed => browser_text_cell(
                                                    entry
                                                        .curation
                                                        .map(|curation| {
                                                            format_last_played(
                                                                curation.last_played_at(),
                                                                now_epoch_seconds,
                                                            )
                                                        })
                                                        .unwrap_or_else(|| "—".to_string()),
                                                    column.width,
                                                    &list_theme,
                                                )
                                                .into_any_element(),
                                            };
                                            sample_row = sample_row.child(cell);
                                        }
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
            .on_action(cx.listener(Self::open_unsupported_action))
            .on_action(cx.listener(Self::close_unsupported_action))
            .on_action(cx.listener(Self::inspect_unsupported_action))
            .capture_any_mouse_down(cx.listener(Self::cancel_file_drag_from_mouse_down))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, window, cx| {
                view.update_column_gesture(
                    f32::from(event.position.x),
                    event.pressed_button,
                    window,
                    cx,
                );
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
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_column_gesture(true, window, cx);
                    view.finish_file_drag_pointer(event, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, window, cx| {
                    view.finish_column_gesture(false, window, cx);
                    view.finish_file_drag_pointer(event, window, cx);
                }),
            )
            .child(
                div()
                    .debug_selector(|| "main-toolbar".into())
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(toolbar_height))
                    .when(enlarged_text, |element| {
                        element.flex_wrap().content_start()
                    })
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
                            .when(enlarged_text, |element| {
                                element.h(px(theme.spacing.control_height))
                            })
                            .window_control_area(WindowControlArea::Drag)
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
                            .id("focus-loaded-toolbar")
                            .relative()
                            .when_some(self.ax_focus("transport-focus-loaded"), |element, focus| {
                                element.track_focus(&focus)
                            })
                            .when_some(
                                self.ax_marker("transport-focus-loaded"),
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
                                control.tooltip(focus_loaded_tooltip)
                            })
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.focus_loaded_sample_in_browser(window, cx)
                            }))
                            .text_size(px(theme.typography.small_size))
                            .text_color(color(theme.colors.text_muted))
                            .child("◎"),
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
                            .id("transient-markers-toolbar")
                            .relative()
                            .when_some(
                                self.ax_focus("transport-transient-markers"),
                                |element, focus| element.track_focus(&focus),
                            )
                            .when_some(
                                self.ax_marker("transport-transient-markers"),
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
                            .on_click(cx.listener(Self::toggle_transient_markers_from_toolbar))
                            .text_size(px(theme.typography.small_size))
                            .text_color(transient_markers_color)
                            .child("I"),
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
                        super::filter_family::render(
                            self,
                            &theme,
                            yield_core::BrowserFilterFamily::Name,
                            cx,
                        )
                        .h(px(theme.spacing.control_height)),
                    )
                    .child(
                        div()
                            .min_w(px(80.0))
                            .w(px(220.0))
                            .debug_selector(|| "browser-search-toolbar".into())
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
                    .child(div().id("open-unsupported-entries").debug_selector(|| "open-unsupported-entries".into())
                        .cursor_pointer().child("Unsupported files")
                        .on_click(cx.listener(Self::open_unsupported_from_toolbar)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .debug_selector(|| "source-root-label".into())
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
                            .debug_selector(|| "audio-output-settings-toolbar".into())
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
                            .debug_selector(|| "reveal-toolbar".into())
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
                            .debug_selector(|| "choose-folder-toolbar".into())
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
                            .debug_selector(|| "shortcut-help-toolbar".into())
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
                element.child(
                    div()
                        .id("settings-panel-scroll")
                        .debug_selector(|| "settings-panel-scroll".into())
                        .flex()
                        .flex_col()
                        .flex_none()
                        .w_full()
                        .max_h(window.viewport_size().height * 0.5)
                        .overflow_y_scroll()
                        .child(self.render_theme_settings(theme.clone(), cx))
                        .child(self.render_general_settings(theme.clone(), cx)),
                )
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
                        window,
                        cx,
                    ))
                    .child(self.render_sidebar_resize_rail(theme.clone(), cx))
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
                            .child(self.render_waveform_resize_rail(theme.clone(), cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .h_full()
                                    .bg(color(theme.colors.surface))
                                    .child(
                                        div()
                                            .id("sample-table-scroll")
                                            .debug_selector(|| "sample-table-viewport".into())
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .w_full()
                                            .min_w_0()
                                            .overflow_x_scroll()
                                            .child(
                                                div()
                                                    .debug_selector(|| "sample-table-content".into())
                                                    .flex()
                                                    .flex_col()
                                                    .h_full()
                                                    .w(px(table_width))
                                                    .child({
                                                        let insertion_marker =
                                                            if let Some(ColumnGesture::Reorder {
                                                                source,
                                                                insertion: Some(insertion),
                                                                ..
                                                            }) = self.column_gesture
                                                            {
                                                                let target = resolved_columns
                                                                    .iter()
                                                                    .filter(|column| {
                                                                        column.id != source
                                                                    })
                                                                    .nth(insertion)
                                                                    .map(|column| column.id);
                                                                let mut left = theme
                                                                    .spacing
                                                                    .selected_marker_width;
                                                                for column in &resolved_columns {
                                                                    if Some(column.id) == target {
                                                                        break;
                                                                    }
                                                                    left += column.width;
                                                                }
                                                                Some(left)
                                                            } else {
                                                                None
                                                            };
                                                        let header_bounds =
                                                            self.column_header_bounds.clone();
                                                        let mut column_header = div()
                                                            .relative()
                                                            .flex()
                                                            .flex_none()
                                                            .items_center()
                                                            .h(px(theme.spacing.row_height))
                                                            .w(px(table_width))
                                                            .child(
                                                                div()
                                                                    .flex_none()
                                                                    .w(px(theme
                                                                        .spacing
                                                                        .selected_marker_width))
                                                                    .h_full()
                                                                    .bg(transparent()),
                                                            );
                                                        for column in &resolved_columns {
                                                            column_header = column_header.child(
                                                                self.render_browser_header_column(
                                                                    *column,
                                                                    name_column_width,
                                                                    theme.clone(),
                                                                    cx,
                                                                ),
                                                            );
                                                        }
                                                        column_header
                                                            .child(
                                                                canvas(
                                                                    move |bounds, _, _| {
                                                                        *header_bounds
                                                                            .borrow_mut() =
                                                                            Some(bounds);
                                                                    },
                                                                    move |_, _, _, _| {},
                                                                )
                                                                .absolute()
                                                                .top_0()
                                                                .bottom_0()
                                                                .left_0()
                                                                .right_0(),
                                                            )
                                                            .when_some(
                                                                insertion_marker,
                                                                |header, left| {
                                                                header.child(
                                                                    div()
                                                                        .absolute()
                                                                        .debug_selector(|| "browser-column-insertion".into())
                                                                            .left(px(left))
                                                                            .top_0()
                                                                            .h_full()
                                                                            .w(px(2.0))
                                                                            .bg(color(
                                                                                theme.colors.accent,
                                                                            )),
                                                                    )
                                                                },
                                                            )
                                                    })
                                                    .child(list),
                                            ),
                                    ),
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
                    .debug_selector(|| "main-status-bar".into())
                    .h(px(status_bar_height))
                    .when(enlarged_text, |element| {
                        element.flex_wrap().content_start().items_start()
                    })
                    .gap(px(theme.spacing.control_gap))
                    .px(px(theme.spacing.panel_padding))
                    .bg(color(theme.colors.surface_raised))
                    .border_t(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border))
                    .text_size(px(theme.typography.small_size))
                    .text_color(color(theme.colors.text_muted))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(px(theme.spacing.control_gap))
                            .overflow_hidden()
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
                            .when_some(self.os_entry_status, |element, status| {
                                element.child(status)
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
                            .child(self.theme_status.clone()),
                    )
                    .child(
                        div()
                            .id("status-job-details")
                            .track_focus(&self.job_details_focus_handle)
                            .debug_selector(|| "status-job-details".into())
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(10.0))
                                    .h(px(10.0))
                                    .rounded_full()
                                    .bg(color(theme.colors.accent))
                                    .opacity(if jobs_running {
                                        f32::from(motion_frame.worker_alpha) / 255.0
                                    } else {
                                        0.35
                                    }),
                            )
                            .max_w(px(280.0))
                            .child(div().min_w_0().text_ellipsis().child(jobs_status))
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.job_details_focus_handle);
                                view.job_details_open = !view.job_details_open;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.job_details_open, |element| {
                let mut panel = div()
                    .id("status-job-details-panel")
                    .debug_selector(|| "status-job-details-panel".into())
                    .absolute()
                    .bottom(px(status_bar_height))
                    .right(px(theme.spacing.panel_padding))
                    .w(px(360.0))
                    .max_w(window.bounds().size.width - px(theme.spacing.panel_padding * 2.0))
                    .max_h(px(300.0))
                    .overflow_y_scroll()
                    .p(px(theme.spacing.panel_padding))
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.control_gap))
                    .bg(color(theme.colors.surface_raised))
                    .border(px(theme.spacing.border_width))
                    .border_color(color(theme.colors.border))
                    .occlude()
                    .child("Job Details");
                if jobs.rows.is_empty() {
                    panel = panel.child("No active jobs");
                }
                if let Some(error) = self.job_action_error.as_ref() {
                    panel = panel.child(error.clone());
                }
                for row in &jobs.rows {
                    let source = row.source_label();
                    panel = panel.child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().min_w_0().text_ellipsis().child(format!("{} · {source}", row.kind.label())))
                            .child(row.state_label())
                            .child(row.work.clone())
                            .when_some(row.blocked_label(), |element, error| element.child(error))
                            .when(row.can_retry, |element| {
                                element.child(
                                    div()
                                        .id("status-job-retry")
                                        .debug_selector(|| "status-job-retry".into())
                                        .cursor_pointer()
                                        .child("Retry")
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.retry_current_job(cx);
                                            window.focus(&view.job_details_focus_handle);
                                        })),
                                )
                            })
                            .when(row.can_cancel, |element| {
                                element.child(
                                    div()
                                        .id("status-job-cancel")
                                        .debug_selector(|| "status-job-cancel".into())
                                        .cursor_pointer()
                                        .child("Cancel")
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.cancel_job_audit(cx);
                                            window.focus(&view.job_details_focus_handle);
                                        })),
                                )
                            }),
                    );
                }
                element.child(panel)
            })
            .when(self.audio_settings_open, |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(toolbar_height))
                        .right_0()
                        .w(px(420.0))
                        .occlude()
                        .child(self.render_audio_output_settings(theme.clone(), cx)),
                )
            })
            .child(super::similarity_list::render(self, &theme, cx))
            .when(self.context_menu.is_some(), |element| {
                element.child(context_menu::render_overlay(self, &theme, cx))
            })
            .when(self.unsupported_entries.is_open(), |element| {
                element.child(super::unsupported_entries::render(self, &theme, window.bounds().size, cx))
            })
            .when(self.shortcut_help.is_open(), |element| {
                element.child(shortcut_help::render(
                    self,
                    theme.clone(),
                    window.bounds().size,
                    cx,
                ))
            })
            .when(self.feedback_issue.open, |element| {
                element.child(feedback_issue::render(self, window.bounds().size, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use gpui::{
        EntityInputHandler, KeyDownEvent, Keystroke, Modifiers, MouseDownEvent, ScrollDelta,
        ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext, point, px,
    };
    use yield_core::{
        AudioFile, BrowserViewModel, WaveformEditFade, WaveformEditSettings, WaveformViewport,
    };
    use yield_media::WaveformPreview;

    use super::super::transport;

    #[gpui::test]
    fn shift_f1_opens_feedback_from_text_focus_and_escape_restores_focus(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.feedback_issue.use_test_service();
                let search_focus = view.search_input.read(cx).focus_handle();
                window.focus(&search_focus);
                let held = KeyDownEvent {
                    keystroke: Keystroke::parse("shift-f1").expect("Shift-F1"),
                    is_held: true,
                };
                assert!(!view.handle_feedback_key(&held, window, cx));
                assert!(!view.feedback_issue.open);
                let press = KeyDownEvent {
                    keystroke: Keystroke::parse("shift-f1").expect("Shift-F1"),
                    is_held: false,
                };
                assert!(view.handle_feedback_key(&press, window, cx));
                assert!(view.feedback_issue.open);
                let escape = KeyDownEvent {
                    keystroke: Keystroke::parse("escape").expect("Escape"),
                    is_held: false,
                };
                assert!(view.handle_feedback_key(&escape, window, cx));
                assert!(!view.feedback_issue.open);
                assert!(search_focus.is_focused(window));
            });
        });
    }

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

    #[gpui::test]
    fn space_replays_selected_span_and_shift_space_plays_from_mark_to_end(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let path = PathBuf::from("/tmp/space-shortcut.wav");
        let region = waveform::WaveformPlayRegion::new(0.2, 0.8).expect("play region");
        install_playmark_state(&browser, cx, &path, region);
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                window.focus(&view.focus_handle);
                view.playing_path = Some(path.clone());
                let press = |view: &mut Browser,
                             key: &str,
                             window: &mut Window,
                             cx: &mut Context<Browser>| {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse(key).expect("shortcut"),
                            is_held: false,
                        },
                        window,
                        cx,
                    );
                };
                press(view, "space", window, cx);
                let intent = view.playback_init_intent.as_ref().expect("replay queued");
                assert_eq!(intent.path, path);
                assert_eq!(intent.region, Some(region));
                assert_eq!(intent.start, region.start);

                press(view, "shift-space", window, cx);
                let intent = view
                    .playback_init_intent
                    .as_ref()
                    .expect("mark play queued");
                assert_eq!(intent.start, region.start);
                assert_eq!(intent.region, None);

                press(view, "shift-c", window, cx);
                assert_eq!(view.playback_status, "Set a compare anchor first");
                assert_eq!(
                    view.playback_init_intent
                        .as_ref()
                        .map(|intent| intent.region),
                    Some(None)
                );

                view.playing_path = None;
                view.waveform = None;
                view.waveform_play_region = None;
                view.playback_init_intent = None;
                press(view, "shift-space", window, cx);
                let intent = view
                    .playback_init_intent
                    .as_ref()
                    .expect("fallback play queued");
                assert_eq!(intent.start, 0.0);
                assert_eq!(intent.region, None);

                view.playback_init_intent = None;
                view.selected = None;
                press(view, "space", window, cx);
                assert_eq!(view.playback_status, "Select a sample first");
                assert!(view.playback_init_intent.is_none());

                view.selected = Some(0);
                view.waveform = Some(inert_preview());
                view.random_audition_sticky = true;
                press(view, "space", window, cx);
                assert_eq!(view.playback_status, "Random audition preview unavailable");
                assert!(view.playback_init_intent.is_none());
            });
        });
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
    fn horizontal_table_scroll_moves_header_and_rows_together(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            for column in BrowserColumn::DEFAULT_ORDER {
                view.column_layout.set_width(column, 420.0);
            }
            let root = PathBuf::from("/tmp/opt1908-scroll-fixture");
            view.root = Some(root.clone());
            view.entries = vec![AudioFile {
                path: root.join("alpha.wav"),
                relative_path: PathBuf::from("alpha.wav"),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.view_model = BrowserViewModel::new(&view.entries);
        });
        cx.refresh().expect("render overflowed table");
        cx.run_until_parked();
        let name_before = cx.debug_bounds("browser-column-Name").expect("name header");
        let row_before = cx
            .debug_bounds("browser-row-name-0")
            .expect("first row name cell");
        let scroll_position = point(
            name_before.origin.x + px(50.0),
            name_before.origin.y + name_before.size.height / 2.0,
        );
        cx.simulate_event(ScrollWheelEvent {
            position: scroll_position,
            delta: ScrollDelta::Pixels(point(px(-200.0), px(0.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.refresh().expect("render horizontally scrolled table");
        let name_after = cx
            .debug_bounds("browser-column-Name")
            .expect("scrolled name header");
        let row_after = cx
            .debug_bounds("browser-row-name-0")
            .expect("scrolled row name cell");
        assert!(name_after.origin.x < name_before.origin.x - px(50.0));
        assert_eq!(
            row_after.origin.x - row_before.origin.x,
            name_after.origin.x - name_before.origin.x
        );
    }

    #[gpui::test]
    fn column_drag_escape_restores_layout(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("render columns");
        cx.run_until_parked();
        let name = cx.debug_bounds("browser-column-Name").expect("name header");
        let rail = cx
            .debug_bounds("browser-column-resize-Name")
            .expect("name resize rail");
        let start = point(
            rail.origin.x + rail.size.width / 2.0,
            rail.origin.y + rail.size.height / 2.0,
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x - px(70.0), start.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.refresh().expect("render resize preview");
        assert!(
            cx.debug_bounds("browser-column-Name")
                .expect("resized name")
                .size
                .width
                < name.size.width
        );
        cx.simulate_keystrokes("escape");
        cx.refresh().expect("render canceled resize");
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(view.column_layout.width_override(BrowserColumn::Name), None);
            assert!(view.column_gesture.is_none());
        });
        assert_eq!(
            cx.debug_bounds("browser-column-Name")
                .expect("restored name")
                .size
                .width,
            name.size.width
        );
        let rating = cx
            .debug_bounds("browser-column-Rating")
            .expect("rating header");
        let size = cx.debug_bounds("browser-column-Size").expect("size header");
        let start = point(
            rating.origin.x + px(20.0),
            rating.origin.y + rating.size.height / 2.0,
        );
        let target = point(size.origin.x + size.size.width - px(10.0), start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
        cx.refresh().expect("render reorder preview");
        assert!(cx.debug_bounds("browser-column-insertion").is_some());
        cx.simulate_keystrokes("escape");
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.column_layout
                    .visible_order(BrowserColumnVisibility::default())[1],
                BrowserColumn::Rating
            );
        });
    }

    #[gpui::test]
    fn column_pointer_resize_and_reorder_commit_on_release(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("render browser columns");
        cx.run_until_parked();
        let rating = cx
            .debug_bounds("browser-column-Rating")
            .expect("rating header");
        let size = cx.debug_bounds("browser-column-Size").expect("size header");
        let resize = cx
            .debug_bounds("browser-column-resize-Rating")
            .expect("rating resize rail");
        let resize_center = point(
            resize.origin.x + resize.size.width / 2.0,
            resize.origin.y + resize.size.height / 2.0,
        );
        cx.simulate_mouse_down(resize_center, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(resize_center.x + px(34.0), resize_center.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(resize_center.x + px(34.0), resize_center.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.refresh().expect("render resized header");
        cx.run_until_parked();
        let resized = cx
            .debug_bounds("browser-column-Rating")
            .expect("resized rating");
        assert!(resized.size.width > rating.size.width + px(25.0));
        assert!(
            resized.origin.x + resized.size.width > rating.origin.x + rating.size.width + px(25.0)
        );

        let rating_start = point(
            resized.origin.x + px(20.0),
            resized.origin.y + resized.size.height / 2.0,
        );
        let size_end = point(size.origin.x + size.size.width + px(12.0), rating_start.y);
        cx.simulate_mouse_down(rating_start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(size_end, MouseButton::Left, Modifiers::none());
        cx.refresh().expect("render insertion marker");
        assert!(cx.debug_bounds("browser-column-insertion").is_some());
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert_eq!(
                view.column_layout
                    .visible_order(BrowserColumnVisibility::default())[1],
                BrowserColumn::Rating
            );
        });
        cx.simulate_mouse_up(size_end, MouseButton::Left, Modifiers::none());
        cx.refresh().expect("render reordered columns");
        cx.cx.update(|app| {
            let view = browser.read(app);
            let order = view
                .column_layout
                .visible_order(BrowserColumnVisibility::default());
            assert!(
                order
                    .iter()
                    .position(|column| *column == BrowserColumn::Rating)
                    .unwrap()
                    > order
                        .iter()
                        .position(|column| *column == BrowserColumn::Size)
                        .unwrap()
            );
            assert_eq!(view.view_model.sort().column, SortColumn::Name);
        });
        let rating = cx
            .debug_bounds("browser-column-Rating")
            .expect("moved rating header");
        let click = point(
            rating.origin.x + px(20.0),
            rating.origin.y + rating.size.height / 2.0,
        );
        cx.simulate_mouse_down(click, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(click, MouseButton::Left, Modifiers::none());
        cx.cx.update(|app| {
            assert_eq!(
                browser.read(app).view_model.sort().column,
                SortColumn::Rating
            )
        });
    }

    #[gpui::test]
    fn enlarged_column_resize_persists_logical_widths(cx: &mut TestAppContext) {
        let enlarged = crate::theme::with_text_scale(&Theme::default(), 200);
        cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            view.general_settings.text_scale_percent = 200;
            view.column_layout.set_width(BrowserColumn::Rating, 120.0);
            cx.notify();
        });
        cx.refresh().expect("render enlarged browser columns");
        cx.run_until_parked();

        let name = cx.debug_bounds("browser-column-Name").expect("name header");
        let rating = cx
            .debug_bounds("browser-column-Rating")
            .expect("rating header");
        let resize = cx
            .debug_bounds("browser-column-resize-Rating")
            .expect("rating resize rail");
        let start = point(
            resize.origin.x + resize.size.width / 2.0,
            resize.origin.y + resize.size.height / 2.0,
        );
        let end = point(start.x + px(34.0), start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.refresh().expect("render enlarged resize");

        let resized = cx
            .debug_bounds("browser-column-Rating")
            .expect("resized rating header");
        assert_eq!(resized.size.width, rating.size.width + px(34.0));
        let (rating_width, name_width) = cx.cx.update(|app| {
            let view = browser.read(app);
            (
                view.column_layout
                    .width_override(BrowserColumn::Rating)
                    .expect("stored rating width"),
                view.column_layout
                    .width_override(BrowserColumn::Name)
                    .expect("frozen name width"),
            )
        });
        assert!(
            (rating_width * 2.0 - f32::from(resized.size.width)).abs() <= 1.0,
            "stored rating width {rating_width} should be logical at 200%: {resized:?}"
        );
        assert!(
            (name_width * 2.0 - f32::from(name.size.width)).abs() <= 1.0,
            "frozen name width {name_width} should remain logical at 200%: {name:?}"
        );
        assert_eq!(rating_width, 137.0);
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
    fn x_uses_zoom_state_across_browser_and_waveform_focus(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let path = PathBuf::from("/tmp/opt1910-x-fixture/sample.wav");
                view.root = path.parent().map(Path::to_path_buf);
                view.entries = vec![AudioFile {
                    path,
                    relative_path: PathBuf::from("sample.wav"),
                    byte_len: 0,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                }];
                view.view_model = BrowserViewModel::new(&view.entries);
                view.selected = Some(0);
                view.waveform = Some(inert_preview());
                let focused_id = view.entries[0].stable_id();
                let zoomed = WaveformViewport::full().zoom_to_region(48_000, 0.25, 0.75);
                let press = |view: &mut Browser,
                             key: &str,
                             held: bool,
                             window: &mut Window,
                             cx: &mut Context<Browser>| {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse(key).expect("X keystroke"),
                            is_held: held,
                        },
                        window,
                        cx,
                    );
                };

                window.focus(&view.focus_handle);
                view.waveform_viewport = zoomed;
                press(view, "x", false, window, cx);
                assert_eq!(view.waveform_viewport, WaveformViewport::full());
                assert!(!view.selection.contains(&focused_id));
                press(view, "x", true, window, cx);
                assert!(!view.selection.contains(&focused_id));

                window.focus(&view.waveform_focus_handle);
                view.waveform_viewport = zoomed;
                press(view, "x", false, window, cx);
                assert_eq!(view.waveform_viewport, WaveformViewport::full());
                assert!(!view.selection.contains(&focused_id));
                press(view, "x", false, window, cx);
                assert!(view.selection.contains(&focused_id));
                assert_eq!(view.browser_focus_id(), Some(focused_id.clone()));

                view.waveform_viewport = zoomed;
                press(view, "alt-x", false, window, cx);
                assert_eq!(view.waveform_viewport, zoomed);
                assert!(view.selection.contains(&focused_id));
                view.waveform = None;
                press(view, "x", false, window, cx);
                assert!(!view.selection.contains(&focused_id));
            });
        });
    }

    #[gpui::test]
    fn locked_loop_shortcut_respects_text_focus_repeat_and_rejected_submission(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let press =
                    |view: &mut Browser, held, window: &mut Window, cx: &mut Context<Browser>| {
                        view.handle_key_down(
                            &KeyDownEvent {
                                keystroke: Keystroke::parse("shift-l").expect("Shift-L"),
                                is_held: held,
                            },
                            window,
                            cx,
                        );
                    };
                let initial_status = view.preference_status().to_string();
                window.focus(&view.search_input.read(cx).focus_handle());
                press(view, false, window, cx);
                assert_eq!(view.preference_status(), initial_status);

                window.focus(&view.waveform_focus_handle);
                press(view, true, window, cx);
                assert_eq!(view.preference_status(), initial_status);
                press(view, false, window, cx);
                assert!(view.preference_status().contains("Settings not saved"));
                assert_eq!(view.playback_policy.locked_override(), None);
            });
        });
    }

    #[gpui::test]
    fn bpm_snap_shortcut_requires_waveform_focus_and_ignores_repeats(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let press =
                    |view: &mut Browser, held, window: &mut Window, cx: &mut Context<Browser>| {
                        view.handle_key_down(
                            &KeyDownEvent {
                                keystroke: Keystroke::parse("b").expect("B key"),
                                is_held: held,
                            },
                            window,
                            cx,
                        );
                    };

                window.focus(&view.waveform_focus_handle);
                press(view, false, window, cx);
                assert!(
                    !view.bpm_snap_enabled,
                    "an unloaded waveform cannot toggle snap"
                );

                view.waveform = Some(inert_preview());
                press(view, false, window, cx);
                assert!(view.bpm_snap_enabled);
                press(view, true, window, cx);
                assert!(view.bpm_snap_enabled, "key repeat must not toggle snap");

                window.focus(&view.focus_handle);
                press(view, false, window, cx);
                assert!(view.bpm_snap_enabled, "browser focus must not toggle snap");

                window.focus(&view.search_input.read(cx).focus_handle());
                press(view, false, window, cx);
                assert!(
                    view.bpm_snap_enabled,
                    "typing in search must not toggle snap"
                );

                window.focus(&view.waveform_focus_handle);
                press(view, false, window, cx);
                assert!(!view.bpm_snap_enabled);
                view.toggle_bpm_snap(cx);
                assert!(
                    view.bpm_snap_enabled,
                    "toolbar and AX use this same transition"
                );
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
    fn general_settings_ax_activation_keeps_previously_focused_input(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let search_focus = view.search_input.read(cx).focus_handle();
                window.focus(&search_focus);
                // AX dispatch captures focus before focusing the settings button.
                view.general_settings.previous_focus = window.focused(cx);
                window.focus(&view.focus_handle);
                view.toggle_theme_settings_from_accessibility(window, cx);
                assert!(view.theme_settings_open);
                assert_eq!(
                    view.general_settings.previous_focus,
                    Some(search_focus.clone())
                );
                view.close_general_settings(window, cx);
                assert!(search_focus.is_focused(window));
            });
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
    fn visual_settings_scroll_keeps_late_actions_reachable_and_footer_visible(
        cx: &mut TestAppContext,
    ) {
        use gpui::{ScrollDelta, ScrollWheelEvent, TouchPhase};
        let enlarged = crate::theme::with_text_scale(&Theme::default(), 200);
        cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        let window = cx.update(|app| {
            app.open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::new(
                        point(px(0.0), px(0.0)),
                        gpui::size(px(960.0), px(560.0)),
                    ))),
                    ..Default::default()
                },
                |_, app| app.new(Browser::new),
            )
            .expect("open constrained settings window")
        });
        let browser = window.root(cx).expect("browser root");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        browser.update(&mut cx, |view, cx| {
            view.theme_settings_open = true;
            view.general_settings.text_scale_percent = 200;
            cx.notify();
        });
        cx.run_until_parked();
        cx.refresh().expect("settings layout");
        let scroll = cx
            .debug_bounds("settings-panel-scroll")
            .expect("settings viewport");
        let before = cx
            .debug_bounds("clear-rebuildable-cache")
            .expect("last settings action");
        assert!(scroll.size.height <= px(280.0));
        cx.simulate_event(ScrollWheelEvent {
            position: point(scroll.left() + px(8.0), scroll.top() + px(8.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-4000.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.refresh().expect("scrolled settings layout");
        let after = cx
            .debug_bounds("clear-rebuildable-cache")
            .expect("reachable last action");
        assert!(after.top() < before.top());
        assert!(
            after.top() >= scroll.top() && after.bottom() <= scroll.bottom(),
            "last action {after:?} outside scroll {scroll:?}"
        );
        let footer = cx.debug_bounds("main-status-bar").expect("footer");
        assert!(
            footer.bottom() <= px(560.0),
            "footer {footer:?} escaped window"
        );
    }

    #[gpui::test]
    fn enlarged_browser_layout_keeps_toolbar_footer_and_columns_in_viewport(
        cx: &mut TestAppContext,
    ) {
        let enlarged = crate::theme::with_text_scale(&Theme::default(), 200);
        cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        let window = cx.update(|app| {
            app.open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::new(
                        point(px(0.0), px(0.0)),
                        gpui::size(px(960.0), px(560.0)),
                    ))),
                    ..Default::default()
                },
                |_, app| app.new(Browser::new),
            )
            .expect("open minimum layout test window")
        });
        let browser = window.root(cx).expect("browser root");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        browser.update(&mut cx, |view, cx| {
            view.general_settings.text_scale_percent = 200;
            let root = PathBuf::from("/tmp/opt1880-enlarged-layout");
            view.root = Some(root.clone());
            view.entries = vec![AudioFile {
                path: root.join("selectable.wav"),
                relative_path: PathBuf::from("selectable.wav"),
                byte_len: 0,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.view_model = BrowserViewModel::new(&view.entries);
            cx.notify();
        });
        cx.run_until_parked();
        cx.refresh().expect("enlarged browser layout");

        let toolbar = cx.debug_bounds("main-toolbar").expect("toolbar bounds");
        let status = cx.debug_bounds("main-status-bar").expect("status bounds");
        let mut escaped_controls = Vec::new();
        for selector in [
            "browser-search-toolbar",
            "browser-name-filter-toggle",
            "source-root-label",
            "audio-output-settings-toolbar",
            "reveal-toolbar",
            "choose-folder-toolbar",
            "theme-settings",
            "shortcut-help-toolbar",
        ] {
            let control = cx.debug_bounds(selector).expect("critical toolbar control");
            if control.left() < toolbar.left()
                || control.right() > toolbar.right()
                || control.top() < toolbar.top()
                || control.bottom() > toolbar.bottom()
            {
                escaped_controls.push(format!("{selector}: {control:?}"));
            }
        }
        assert!(
            escaped_controls.is_empty(),
            "{escaped_controls:?}; toolbar: {toolbar:?}"
        );
        let random = cx
            .debug_bounds("random-audition-toolbar")
            .expect("random audition bounds");
        let settings = cx.debug_bounds("theme-settings").expect("settings bounds");
        assert!(
            settings.top() > random.top(),
            "toolbar did not wrap into two rows"
        );
        assert!(toolbar.size.height > px(70.0), "{toolbar:?}");
        assert!(
            status.top() >= toolbar.bottom(),
            "{status:?} follows {toolbar:?}"
        );
        assert!(
            status.bottom() <= px(560.0),
            "footer escaped the window: {status:?}"
        );
        assert!(
            status.size.height > px(60.0),
            "footer did not reserve wrapped text: {status:?}"
        );

        let viewport = cx
            .debug_bounds("sample-table-viewport")
            .expect("sample table viewport");
        let heading = cx
            .debug_bounds("browser-column-Name")
            .expect("sample name column header");
        let row = cx
            .debug_bounds("browser-row-name-0")
            .expect("selectable sample row");
        let content = cx
            .debug_bounds("sample-table-content")
            .expect("sample table content");
        assert!(
            viewport.size.height >= heading.size.height + row.size.height,
            "enlarged toolbar left no room for a header and selectable sample row: viewport {viewport:?}, heading {heading:?}, row {row:?}"
        );
        assert!(row.top() >= viewport.top() && row.bottom() <= viewport.bottom());
        assert!(
            row.bottom() <= status.top(),
            "row overlaps footer: {row:?}, {status:?}"
        );
        assert!(
            content.size.width > viewport.size.width,
            "enlarged columns should use the existing horizontal viewport: {content:?}, {viewport:?}"
        );
        assert!(
            heading.size.width >= px(384.0),
            "name column did not scale: {heading:?}"
        );
        let anchor = cx
            .debug_bounds("sample-similarity-anchor-0")
            .expect("leading anchor control");
        cx.simulate_mouse_down(anchor.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(anchor.center(), MouseButton::Left, Modifiers::none());
        browser.update(&mut cx, |view, _| assert_eq!(view.selected, None));
        let row_center = cx
            .debug_bounds("browser-row-label-0")
            .expect("sample label")
            .center();
        cx.simulate_mouse_down(row_center, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(row_center, MouseButton::Left, Modifiers::none());
        browser.update(&mut cx, |view, _| assert_eq!(view.selected, Some(0)));
    }

    #[gpui::test]
    fn enlarged_playback_age_chips_fit_constrained_sidebar(cx: &mut TestAppContext) {
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
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.refresh().expect("enlarged age filter layout");
        let row = cx
            .debug_bounds("playback-age-filter-row")
            .expect("age filter row");
        let chips = [
            "playback-age-chip-0",
            "playback-age-chip-1",
            "playback-age-chip-2",
        ]
        .into_iter()
        .map(|selector| cx.debug_bounds(selector).expect("age chip"))
        .collect::<Vec<_>>();
        for chip in &chips {
            assert!(chip.origin.x >= row.origin.x);
            assert!(chip.origin.x + chip.size.width <= row.origin.x + row.size.width);
            assert!(chip.origin.y + chip.size.height <= row.origin.y + row.size.height);
        }
        assert!(
            chips
                .windows(2)
                .all(|pair| { pair[0].origin.x + pair[0].size.width <= pair[1].origin.x })
        );
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
    fn workspace_pane_sizes_clamp_to_window_and_restore_preferred_sizes() {
        let theme = Theme::default();
        assert_eq!(sidebar_width(Some(420.0), 1_280.0, &theme), 420.0);
        assert!(sidebar_width(Some(420.0), 480.0, &theme) < 420.0);
        assert_eq!(sidebar_width(Some(420.0), 1_280.0, &theme), 420.0);
        assert_eq!(sidebar_width(Some(0.0), 1_280.0, &theme), 180.0);

        assert_eq!(
            waveform_height(
                Some(512.0),
                1_200.0,
                &theme,
                theme.spacing.toolbar_height,
                theme.spacing.status_height,
            ),
            512.0
        );
        assert!(
            waveform_height(
                Some(512.0),
                560.0,
                &theme,
                theme.spacing.toolbar_height,
                theme.spacing.status_height,
            ) < 512.0
        );
        assert_eq!(
            waveform_height(
                Some(512.0),
                1_200.0,
                &theme,
                theme.spacing.toolbar_height,
                theme.spacing.status_height,
            ),
            512.0
        );
        let enlarged = crate::theme::with_text_scale(&theme, 200);
        let minimum = enlarged.waveform.title_height
            + enlarged.waveform.metadata_height
            + enlarged.waveform.scrollbar_height
            + enlarged.spacing.waveform_min_bar_height;
        assert!(
            waveform_height(
                Some(94.0),
                560.0,
                &enlarged,
                browser_toolbar_height(960.0, &enlarged, true),
                browser_status_bar_height(&enlarged, true),
            ) >= minimum
        );
    }

    #[gpui::test]
    fn sidebar_resize_preserves_text_focus_and_escape_restores_preference(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("initial layout");
        cx.update(|window, app| {
            window.focus(&browser.read(app).search_input.read(app).focus_handle());
        });
        let rail = cx
            .debug_bounds("workspace-sidebar-resize")
            .expect("sidebar rail");
        let start = rail.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x + px(4.0), start.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(start.x + px(70.0), start.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.update(|window, app| {
            let view = browser.read(app);
            assert!(
                view.search_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
            assert!(view.preferred_sidebar_width.is_some());
        });
        cx.simulate_keystrokes("escape");
        cx.update(|_, app| {
            let view = browser.read(app);
            assert!(view.pane_resize_drag.is_none());
            assert!(view.preferred_sidebar_width.is_none());
        });
    }

    #[gpui::test]
    fn sidebar_resize_retires_on_viewport_or_text_scale_change(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("initial layout");
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_sidebar_width = Some(300.0);
                view.begin_pane_resize(PaneAxis::Sidebar, point(px(300.0), px(100.0)), window, cx);
                view.update_pane_resize(point(px(400.0), px(100.0)), window, cx);
                assert_eq!(view.preferred_sidebar_width, Some(400.0));
            });
        });
        cx.simulate_resize(gpui::size(px(800.0), px(560.0)));
        cx.refresh().expect("resized window");
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.pane_resize_drag.is_none());
                assert_eq!(view.preferred_sidebar_width, Some(300.0));
                view.begin_pane_resize(PaneAxis::Sidebar, point(px(300.0), px(100.0)), window, cx);
            });
            let enlarged = crate::theme::with_text_scale(active(app), 200);
            app.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        });
        cx.refresh().expect("enlarged layout");
        cx.update(|_, app| {
            assert!(browser.read(app).pane_resize_drag.is_none());
            assert_eq!(browser.read(app).preferred_sidebar_width, Some(300.0));
        });
    }

    #[test]
    fn sidebar_clamp_preserves_main_viewport_at_both_text_scales() {
        for percent in [100, 200] {
            let theme = crate::theme::with_text_scale(&Theme::default(), percent);
            for width in [400.0, 800.0, 1440.0] {
                for request in [-500.0, 10000.0, f32::NAN] {
                    let sidebar = sidebar_width(Some(request), width, &theme);
                    assert!(sidebar.is_finite());
                    assert!(sidebar >= 96.0);
                    assert!(sidebar <= MAX_SIDEBAR_WIDTH);
                    assert!(width - sidebar - PANE_RESIZE_RAIL >= MIN_MAIN_PANE_WIDTH);
                }
            }
        }
    }

    #[gpui::test]
    fn workspace_pane_drag_and_cancel_preserve_browser_state(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let selected = view.selected;
                let playback_epoch = view.playback_epoch;
                let start = point(px(300.0), px(220.0));

                view.begin_pane_resize(PaneAxis::Sidebar, start, window, cx);
                view.update_pane_resize(point(px(380.0), px(220.0)), window, cx);
                assert!(view.preferred_sidebar_width.is_some());
                assert_eq!(view.selected, selected);
                assert_eq!(view.playback_epoch, playback_epoch);
                view.cancel_pane_resize(cx);
                assert!(view.preferred_sidebar_width.is_none());

                view.begin_pane_resize(PaneAxis::Waveform, start, window, cx);
                view.finish_pane_resize(point(px(300.0), px(320.0)), window, cx);
                assert!(view.preferred_waveform_height.is_some());
                assert!(view.pane_resize_drag.is_none());
                assert_eq!(view.selected, selected);
                assert_eq!(view.playback_epoch, playback_epoch);
            });
        });
    }

    #[gpui::test]
    fn workspace_resize_focus_loss_restores_preference(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("install focus observer");
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, app| browser.update(app, |view, _| window.focus(&view.focus_handle)));
        cx.refresh().expect("focus browser");
        cx.run_until_parked();
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_sidebar_width = Some(260.0);
                view.begin_pane_resize(PaneAxis::Sidebar, point(px(260.0), px(200.0)), window, cx);
                view.update_pane_resize(point(px(390.0), px(200.0)), window, cx);
                window.blur();
            })
        });
        cx.run_until_parked();
        cx.refresh().expect("dispatch focus loss");
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.pane_resize_drag.is_none());
            assert_eq!(view.preferred_sidebar_width, Some(260.0));
        });
    }

    #[gpui::test]
    fn workspace_resize_window_deactivation_restores_preference(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("install activation observer");
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_sidebar_width = Some(260.0);
                view.begin_pane_resize(PaneAxis::Sidebar, point(px(260.0), px(200.0)), window, cx);
                view.update_pane_resize(point(px(390.0), px(200.0)), window, cx);
            })
        });
        let other = cx.cx.add_window(|_, cx| Browser::new(cx));
        other
            .update(&mut cx.cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.pane_resize_drag.is_none());
            assert_eq!(view.preferred_sidebar_width, Some(260.0));
        });
    }

    #[gpui::test]
    fn workspace_resize_rails_follow_pointer_drags(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("initial workspace layout");
        cx.run_until_parked();
        let sidebar = cx
            .debug_bounds("workspace-sidebar-resize")
            .expect("sidebar resize rail");
        let start = point(
            sidebar.origin.x + sidebar.size.width / 2.0,
            sidebar.origin.y + sidebar.size.height / 2.0,
        );
        let end = point(start.x + px(80.0), start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.cx
            .update(|app| assert!(browser.read(app).pane_resize_drag.is_some()));
        cx.simulate_mouse_move(
            point(start.x + px(4.0), start.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.refresh().expect("sidebar drag layout");
        let moved_sidebar = cx
            .debug_bounds("workspace-sidebar-resize")
            .expect("moved sidebar rail");
        let preferred = cx
            .cx
            .update(|app| browser.read(app).preferred_sidebar_width);
        assert!(
            moved_sidebar.origin.x > sidebar.origin.x + px(40.0),
            "sidebar={sidebar:?} moved={moved_sidebar:?} preferred={:?}",
            preferred
        );
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.pane_resize_drag.is_none());
            assert!(view.preferred_sidebar_width.is_some());
        });

        let waveform = cx
            .debug_bounds("workspace-waveform-resize")
            .expect("waveform resize rail");
        let start = point(
            waveform.origin.x + waveform.size.width / 2.0,
            waveform.origin.y + waveform.size.height / 2.0,
        );
        let end = point(start.x, start.y + px(60.0));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.cx
            .update(|app| assert!(browser.read(app).pane_resize_drag.is_some()));
        cx.simulate_mouse_move(
            point(start.x, start.y + px(4.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.refresh().expect("waveform drag layout");
        let moved_waveform = cx
            .debug_bounds("workspace-waveform-resize")
            .expect("moved waveform rail");
        assert!(moved_waveform.origin.y > waveform.origin.y + px(30.0));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.cx.update(|app| {
            let view = browser.read(app);
            assert!(view.pane_resize_drag.is_none());
            assert!(view.preferred_waveform_height.is_some());
            assert!(view.selected.is_none());
            assert!(view.waveform_play_region.is_none());
        });
    }

    #[test]
    fn playback_type_labels_keep_unknown_and_recognized_values_compact() {
        assert_eq!(playback_type_label(Some(PlaybackType::Loop)), "Loop");
        assert_eq!(playback_type_label(Some(PlaybackType::OneShot)), "1-Shot");
        assert_eq!(playback_type_label(None), "—");
    }

    #[gpui::test]
    fn job_details_fit_compact_and_normal_windows(cx: &mut TestAppContext) {
        use gpui::{AvailableSpace, size};
        use yield_session::FeatureAnalysisPhase as Phase;
        use yield_session::{FeatureAnalysisBlocker as Blocker, FeatureAnalysisState as State};
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        for (width, height) in [(960.0, 560.0), (1280.0, 800.0)] {
            cx.simulate_resize(size(px(width), px(height)));
            for state in [
                State::Preparing {
                    phase: Phase::ComparingReadiness,
                },
                State::Running,
                State::Blocked {
                    reason: Blocker::Computation,
                },
                State::Blocked {
                    reason: Blocker::UnsupportedPlatform,
                },
            ] {
                browser.update(cx, |view, _| {
                    super::super::analysis::tests::install(view, state);
                    view.job_details_open = true;
                    view.source_registry_status = "Source status ".repeat(100);
                });
                cx.draw(
                    point(px(0.0), px(0.0)),
                    size(
                        AvailableSpace::Definite(px(width)),
                        AvailableSpace::Definite(px(height)),
                    ),
                    |_, _| browser.clone(),
                );
                let control = cx
                    .debug_bounds("status-job-details")
                    .expect("job details status control");
                let panel = cx
                    .debug_bounds("status-job-details-panel")
                    .expect("job details panel");
                assert!(control.right() <= px(width));
                assert!(control.bottom() <= px(height));
                assert!(control.size.width <= px(280.0));
                assert!(panel.left() >= px(0.0));
                assert!(panel.right() <= px(width));
                assert!(panel.top() >= px(0.0));
                assert!(panel.bottom() <= control.top());
                if browser.update(cx, |view, _| view.feature_job_row().unwrap().can_retry) {
                    let retry = cx
                        .debug_bounds("status-job-retry")
                        .expect("feature retry control");
                    assert!(retry.left() >= panel.left() && retry.right() <= panel.right());
                }
            }
        }
    }

    #[gpui::test]
    fn job_details_does_not_duplicate_focused_keyboard_click(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let enter = KeyDownEvent {
                    keystroke: Keystroke::parse("enter").expect("Enter"),
                    is_held: false,
                };
                window.focus(&view.job_details_focus_handle);
                view.handle_key_down(&enter, window, cx);
                assert!(!view.job_details_open);
                view.job_details_open = true;
                let escape = KeyDownEvent {
                    keystroke: Keystroke::parse("escape").expect("Escape"),
                    is_held: false,
                };
                window.focus(&view.search_input.read(cx).focus_handle());
                view.handle_key_down(&escape, window, cx);
                assert!(view.job_details_open);
                window.focus(&view.job_details_focus_handle);
                view.handle_key_down(&escape, window, cx);
                assert!(!view.job_details_open);
            });
        });
    }
}
