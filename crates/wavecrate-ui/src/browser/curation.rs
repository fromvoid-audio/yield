#![deny(missing_docs)]
#![deny(warnings)]

//! Passive curation metadata projection and source-scoped collection filters.

use gpui::{ClickEvent, Context, div, prelude::*, px};
use wavecrate_core::{AudioFile, COLLECTION_COUNT};
use wavecrate_design::Theme;

use crate::theme::{color, transparent};

use super::Browser;

/// Render the six fixed collection filters for the active source.
pub(super) fn render_collection_filters(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let active_source = browser.active_source.is_some();
    let active_filter = browser.view_model.collection_filter();
    let counts = browser.collection_filter_counts;
    let rows = (0..COLLECTION_COUNT)
        .map(|slot| {
            let selected = active_filter == Some(slot);
            let slot_color = color(theme.colors.collection_slots[slot]);
            let hover_fill = color(theme.colors.selection_hover_fill);
            let pressed_fill = color(theme.interaction.pressed_fill);
            let label = format!("Collection {}", slot + 1);
            div()
                .id(("collection-filter", slot))
                .flex()
                .flex_none()
                .w_full()
                .h(px(theme.spacing.row_height))
                .items_center()
                .cursor_pointer()
                .bg(if selected {
                    color(theme.colors.selection_fill)
                } else {
                    transparent()
                })
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed_fill))
                .when(!active_source, |element| element.opacity(0.45))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if active_source {
                        view.toggle_collection_filter(slot, cx);
                    }
                }))
                .child(
                    div()
                        .flex_none()
                        .ml(px(theme.spacing.row_padding_x))
                        .mr(px(theme.spacing.control_gap))
                        .w(px(8.0))
                        .h(px(8.0))
                        .bg(slot_color),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(theme.typography.small_size))
                        .text_color(if selected {
                            color(theme.colors.text_primary)
                        } else {
                            color(theme.colors.text_muted)
                        })
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .mr(px(theme.spacing.row_padding_x))
                        .text_size(px(theme.typography.small_size))
                        .text_color(color(theme.colors.text_disabled))
                        .child(counts[slot].to_string()),
                )
        })
        .collect::<Vec<_>>();
    div()
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
                .flex_none()
                .items_center()
                .h(px(theme.spacing.row_height))
                .px(px(theme.spacing.panel_padding))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .child("SOURCE COLLECTIONS"),
        )
        .children(rows)
}

/// Return the compact rating cell representation for a row.
pub(super) fn rating_cell(entry: &AudioFile, theme: &Theme, width: f32) -> impl gpui::IntoElement {
    let Some(curation) = entry.curation else {
        return div()
            .flex()
            .items_center()
            .h_full()
            .flex_none()
            .w(px(width))
            .px(px(theme.spacing.control_gap))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_disabled))
            .child("—");
    };
    let rating = curation.rating();
    let magnitude = usize::from(rating.value().unsigned_abs());
    let marker_color = if rating.value() < 0 {
        theme.colors.rating_trash
    } else if rating.value() > 0 {
        theme.colors.rating_keep
    } else {
        theme.colors.rating_neutral
    };
    let show_lock = rating.locked() && rating.value() > 0;
    let marker_count = magnitude + usize::from(show_lock);
    let content_width = magnitude as f32 * 5.0
        + if show_lock { 9.0 } else { 0.0 }
        + marker_count.saturating_sub(1) as f32 * 2.0;
    let padding = theme
        .spacing
        .control_gap
        .min(((width - content_width) / 2.0).max(0.0));
    let mut markers = Vec::with_capacity(marker_count);
    for _ in 0..magnitude {
        markers.push(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded_full()
                .bg(color(marker_color)),
        );
    }
    if show_lock {
        markers.push(
            div()
                .flex_none()
                .ml(px(2.0))
                .w(px(7.0))
                .h(px(7.0))
                .border(px(theme.spacing.border_width.min(3.5)))
                .border_color(color(theme.colors.rating_keep)),
        );
    }
    div()
        .flex()
        .flex_none()
        .w(px(width))
        .items_center()
        .h_full()
        .gap(px(2.0))
        .px(px(padding))
        .children(markers)
}

/// Return the compact collection swatches for a row.
pub(super) fn collection_cell(
    entry: &AudioFile,
    theme: &Theme,
    width: f32,
) -> impl gpui::IntoElement {
    const MIN_SWATCH_SIZE: f32 = 2.0;
    let width = width.max(0.0);
    let max_padding = ((width - MIN_SWATCH_SIZE * COLLECTION_COUNT as f32) / 2.0).max(0.0);
    let padding = theme.spacing.control_gap.min(max_padding);
    let available = (width - padding * 2.0).max(0.0);
    let swatch_size = (available / COLLECTION_COUNT as f32).clamp(MIN_SWATCH_SIZE, 6.0);
    let gap = if swatch_size >= 6.0 {
        ((available - swatch_size * COLLECTION_COUNT as f32) / (COLLECTION_COUNT - 1) as f32)
            .clamp(0.0, 3.0)
    } else {
        0.0
    };
    let mut swatches = Vec::new();
    if let Some(curation) = entry.curation {
        for slot in 0..COLLECTION_COUNT {
            if curation.has_collection(slot) {
                swatches.push(
                    div()
                        .flex_none()
                        .w(px(swatch_size))
                        .h(px(swatch_size))
                        .bg(color(theme.colors.collection_slots[slot])),
                );
            }
        }
    }
    div()
        .flex()
        .flex_none()
        .w(px(width))
        .items_center()
        .h_full()
        .gap(px(gap))
        .px(px(padding))
        .children(swatches)
}

impl Browser {
    pub(super) fn refresh_collection_filter_counts(&mut self) {
        self.collection_filter_counts = self.view_model.collection_counts(&self.entries);
    }

    pub(super) fn reset_collection_filter(&mut self) {
        self.view_model.set_collection_filter(&self.entries, None);
        self.refresh_collection_filter_counts();
    }

    pub(super) fn toggle_collection_filter(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.active_source.is_none() {
            return;
        }
        let previous_id = self.selected_id();
        let previous_position = self.selected_visible_position();
        if !self
            .view_model
            .toggle_collection_filter(&self.entries, slot)
        {
            return;
        }
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }
}
