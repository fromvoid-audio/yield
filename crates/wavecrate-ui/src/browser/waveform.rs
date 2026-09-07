#![deny(missing_docs)]
#![deny(warnings)]

//! Waveform shaping and canvas rendering for the browser preview panel.

use gpui::{PathBuilder, canvas, div, point, prelude::*, px};
use wavecrate_design::Theme;

use crate::theme::color;

use super::Browser;

fn build_waveform_path(
    bands: &[[f32; 4]],
    lane: usize,
    peak_scale: f32,
    left: gpui::Pixels,
    center: gpui::Pixels,
    width: gpui::Pixels,
    half_height: gpui::Pixels,
) -> Option<PathBuilder> {
    if bands.len() < 2 {
        return None;
    }
    let mut builder = PathBuilder::fill();
    builder.move_to(point(left, center));
    for (index, column) in bands.iter().enumerate() {
        let progress = index as f32 / (bands.len() - 1) as f32;
        let amplitude = waveform_extent(*column, lane, peak_scale);
        builder.line_to(point(
            left + width * progress,
            center - half_height * amplitude,
        ));
    }
    for (index, column) in bands.iter().enumerate().rev() {
        let progress = index as f32 / (bands.len() - 1) as f32;
        let amplitude = waveform_extent(*column, lane, peak_scale);
        builder.line_to(point(
            left + width * progress,
            center + half_height * amplitude,
        ));
    }
    builder.close();
    Some(builder)
}

fn waveform_extent(column: [f32; 4], lane: usize, peak_scale: f32) -> f32 {
    const BAND_SCALES: [f32; 4] = [0.93, 0.43, 0.046, 0.02];
    const BAND_GAINS: [f32; 4] = [1.02, 1.10, 2.08, 0.12];
    const BAND_GAMMA: [f32; 4] = [1.03, 0.94, 0.42, 1.70];

    let signals = [
        column[0].clamp(0.0, 1.0),
        column[1].clamp(0.0, 1.0),
        column[2].clamp(0.0, 1.0),
        column[3].clamp(0.0, 1.0),
    ];
    let raw_signal = signals[3];
    let display_peak = (raw_signal * 1.02).clamp(0.0, 1.0).powf(0.54);
    let raw_carrier = smoothstep(0.010, 0.55, display_peak);
    let low_signal = signals[0];
    let mid_signal = signals[1];
    let high_signal = signals[2];
    let low_peak_ownership = smoothstep(0.10, 0.42, low_signal);
    let mid_dominance = smoothstep(0.18, 0.54, mid_signal) * (1.0 - low_peak_ownership * 0.55);
    let high_dominance = smoothstep(0.10, 0.30, high_signal) * (1.0 - low_peak_ownership * 0.80);

    let noise_floor = if lane == 1 || lane == 2 {
        0.00065
    } else {
        0.004
    };
    let visible_peak = if signals[lane] < noise_floor {
        0.0
    } else {
        signals[lane]
    };
    let shaped_peak = (visible_peak * BAND_GAINS[lane])
        .clamp(0.0, 1.0)
        .powf(BAND_GAMMA[lane]);
    let mut extent = shaped_peak * BAND_SCALES[lane] * 0.86;

    match lane {
        0 => {
            let low_carrier = smoothstep(0.030, 0.28, low_signal) * raw_carrier;
            extent = extent.max(display_peak * 0.90 * low_carrier);
            extent = extent.min(display_peak * 0.96);
        }
        1 => {
            let quiet_presence = smoothstep(0.0007, 0.018, visible_peak);
            let mid_carrier = smoothstep(0.012, 0.24, mid_signal) * raw_carrier;
            let mid_extent_limit = lerp(0.58, 0.86, mid_dominance);
            let mid_extent_target = lerp(0.50, 0.80, mid_dominance);
            extent = extent.max(quiet_presence * 0.010);
            extent = extent.max(display_peak * mid_extent_target * mid_carrier);
            extent = extent.min(display_peak * mid_extent_limit);
        }
        2 => {
            let quiet_presence = smoothstep(0.0007, 0.014, visible_peak);
            let high_carrier = smoothstep(0.006, 0.16, high_signal) * raw_carrier;
            let high_extent_target = lerp(0.080, 0.86, high_dominance);
            extent = extent.max(quiet_presence * 0.0036);
            extent = extent.max(display_peak * high_extent_target * high_carrier);
        }
        _ => {}
    }

    (extent * peak_scale).clamp(0.0, 1.0)
}

fn smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    let t = ((value - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp(start: f32, end: f32, amount: f32) -> f32 {
    start + (end - start) * amount.clamp(0.0, 1.0)
}

/// Render the selected sample's bounded preview using semantic waveform colors.
pub(super) fn render_waveform(browser: &Browser, theme: Theme) -> impl IntoElement {
    let mut bars = div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.waveform_bar_gap))
        .h_full()
        .w_full()
        .overflow_hidden()
        .bg(color(theme.colors.waveform_background));
    if let Some(preview) = browser.waveform.as_ref() {
        if !preview.bands.is_empty() {
            let bands = preview.bands.clone();
            let lane_colors = [
                color(theme.colors.waveform_low),
                color(theme.colors.waveform_mid),
                color(theme.colors.waveform_high),
                color(theme.colors.waveform_raw),
            ];
            let peak_scale = theme.waveform.peak_scale;
            let waveform = canvas(
                move |_, _, _| (),
                move |bounds, _, window, _| {
                    let center = bounds.origin.y + bounds.size.height / 2.0;
                    let half_height = (bounds.size.height / 2.0 - px(2.0)).max(px(1.0));
                    let width = bounds.size.width;
                    for lane in 0..4 {
                        let Some(path) = build_waveform_path(
                            &bands,
                            lane,
                            peak_scale,
                            bounds.origin.x,
                            center,
                            width,
                            half_height,
                        ) else {
                            continue;
                        };
                        if let Ok(path) = path.build() {
                            window.paint_path(path, lane_colors[lane]);
                        }
                    }
                },
            )
            .size_full();
            bars = bars.child(waveform);
        } else {
            let available_height = (theme.spacing.waveform_height
                - theme.waveform.title_height
                - theme.waveform.metadata_height)
                .max(theme.spacing.waveform_min_bar_height);
            let bar_color = if browser.playing_path.is_some() {
                theme.colors.waveform_played
            } else {
                theme.colors.waveform_unplayed
            };
            for peak in &preview.peaks {
                bars = bars.child(
                    div()
                        .flex_1()
                        .h(px((peak * theme.waveform.peak_scale * available_height)
                            .max(theme.spacing.waveform_min_bar_height)))
                        .bg(color(bar_color)),
                );
            }
        }
    } else {
        bars = bars.child(
            div()
                .flex_1()
                .h(px(theme.spacing.waveform_min_bar_height))
                .bg(color(theme.colors.waveform_grid)),
        );
    }
    bars
}
