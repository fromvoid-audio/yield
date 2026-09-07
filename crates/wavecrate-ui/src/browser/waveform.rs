#![deny(missing_docs)]
#![deny(warnings)]

//! Waveform shaping and canvas rendering for the browser preview panel.

use std::{cell::RefCell, rc::Rc};

use gpui::{Bounds, Path, PathBuilder, Pixels, canvas, div, fill, point, prelude::*, px};
use wavecrate_design::Theme;

use crate::theme::color;

use super::Browser;

fn build_waveform_path(
    extents: &[[f32; 4]],
    lane: usize,
    left: gpui::Pixels,
    center: gpui::Pixels,
    width: gpui::Pixels,
    half_height: gpui::Pixels,
) -> Option<PathBuilder> {
    if extents.len() < 2 {
        return None;
    }
    let mut builder = PathBuilder::fill();
    builder.move_to(point(left, center));
    for (index, extent) in extents.iter().enumerate() {
        let progress = index as f32 / (extents.len() - 1) as f32;
        builder.line_to(point(
            left + width * progress,
            center - half_height * extent[lane],
        ));
    }
    for (index, extent) in extents.iter().enumerate().rev() {
        let progress = index as f32 / (extents.len() - 1) as f32;
        builder.line_to(point(
            left + width * progress,
            center + half_height * extent[lane],
        ));
    }
    builder.close();
    Some(builder)
}

/// Retained CPU geometry for the static selected-sample waveform.
///
/// GPUI canvas closures are recreated with the surrounding view, so this cache owns the paths
/// independently of an individual canvas element. The key includes every input that changes the
/// path coordinates or shaped envelope.
#[derive(Clone, Default)]
pub(super) struct WaveformRenderCache {
    state: Rc<RefCell<WaveformRenderCacheState>>,
}

#[derive(Default)]
struct WaveformRenderCacheState {
    key: Option<WaveformCacheKey>,
    paths: Vec<(usize, Path<Pixels>)>,
    #[cfg(test)]
    rebuild_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WaveformCacheKey {
    generation: u64,
    bounds: Bounds<Pixels>,
    peak_scale_bits: u32,
}

impl WaveformRenderCache {
    /// Return paths for this exact preview generation and layout, rebuilding only on a key miss.
    fn paths_for(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
    ) -> Vec<(usize, Path<Pixels>)> {
        let key = WaveformCacheKey {
            generation,
            bounds,
            peak_scale_bits: peak_scale.to_bits(),
        };
        let mut state = self.state.borrow_mut();
        if state.key != Some(key) {
            state.paths = build_waveform_paths(bands, peak_scale, bounds);
            state.key = Some(key);
            #[cfg(test)]
            {
                state.rebuild_count += 1;
            }
        }
        state.paths.clone()
    }

    #[cfg(test)]
    fn rebuild_count(&self) -> usize {
        self.state.borrow().rebuild_count
    }
}

fn build_waveform_paths(
    bands: &[[f32; 4]],
    peak_scale: f32,
    bounds: Bounds<Pixels>,
) -> Vec<(usize, Path<Pixels>)> {
    let point_count = drawable_point_count(bounds.size.width, bands.len());
    let extents = resample_extents(bands, point_count, peak_scale);
    let center = bounds.origin.y + bounds.size.height / 2.0;
    let half_height = (bounds.size.height / 2.0 - px(2.0)).max(px(1.0));
    let width = bounds.size.width;
    (0..4)
        .filter_map(|lane| {
            build_waveform_path(&extents, lane, bounds.origin.x, center, width, half_height)
                .and_then(|builder| builder.build().ok())
                .map(|path| (lane, path))
        })
        .collect()
}

fn drawable_point_count(width: Pixels, available_points: usize) -> usize {
    if available_points < 2 {
        return available_points;
    }
    let width = f32::from(width).ceil().max(2.0) as usize;
    width.min(available_points)
}

/// Downsample to drawable columns after shaping each original column.
///
/// Taking each lane's maximum shaped extent preserves every bucket's visual peak while avoiding
/// sub-pixel path segments the panel cannot display.
fn resample_extents(bands: &[[f32; 4]], point_count: usize, peak_scale: f32) -> Vec<[f32; 4]> {
    if bands.is_empty() || point_count == 0 {
        return Vec::new();
    }
    let point_count = point_count.min(bands.len());
    let mut extents = vec![[0.0_f32; 4]; point_count];
    for (index, column) in bands.iter().copied().enumerate() {
        let bucket = index * point_count / bands.len();
        for lane in 0..4 {
            extents[bucket][lane] =
                extents[bucket][lane].max(waveform_extent(column, lane, peak_scale));
        }
    }
    extents
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
pub(super) fn render_waveform(
    browser: &Browser,
    theme: Theme,
    playback_cursor_ratio: Option<f32>,
) -> impl IntoElement {
    let mut bars = div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.waveform_bar_gap))
        .h_full()
        .w_full()
        .overflow_hidden()
        .relative()
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
            let generation = browser.waveform_generation;
            let cache = browser.waveform_render_cache.clone();
            let waveform = canvas(
                move |_, _, _| (),
                move |bounds, _, window, _| {
                    for (lane, path) in cache
                        .paths_for(generation, &bands, peak_scale, bounds)
                        .into_iter()
                    {
                        window.paint_path(path, lane_colors[lane]);
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
    if let Some(ratio) = playback_cursor_ratio {
        bars = bars.child(playback_cursor_overlay(
            ratio,
            color(theme.colors.waveform_playhead),
        ));
    }
    bars
}

const PLAYBACK_CURSOR_WIDTH: f32 = 2.0;

fn playback_cursor_overlay(ratio: f32, cursor_color: gpui::Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let cursor_width = PLAYBACK_CURSOR_WIDTH.min(width);
            let center = width * ratio.clamp(0.0, 1.0);
            let left = (center - cursor_width / 2.0).clamp(0.0, width - cursor_width);
            let cursor_bounds = Bounds::from_corners(
                point(bounds.left() + px(left), bounds.top()),
                point(bounds.left() + px(left + cursor_width), bounds.bottom()),
            );
            window.paint_quad(fill(cursor_bounds, cursor_color));
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(width), px(height)))
    }

    #[test]
    fn resampling_is_limited_to_drawable_columns_and_preserves_lane_extrema() {
        let bands = (0..2_048)
            .map(|index| {
                let peak = if index == 1_023 {
                    1.0
                } else {
                    index as f32 / 4_096.0
                };
                [peak, peak * 0.8, peak * 0.6, peak]
            })
            .collect::<Vec<_>>();

        let extents = resample_extents(&bands, drawable_point_count(px(320.0), bands.len()), 1.0);

        assert_eq!(extents.len(), 320);
        for lane in 0..4 {
            let source_peak = bands
                .iter()
                .map(|column| waveform_extent(*column, lane, 1.0))
                .fold(0.0, f32::max);
            let rendered_peak = extents
                .iter()
                .map(|column| column[lane])
                .fold(0.0, f32::max);
            assert_eq!(rendered_peak, source_peak);
        }
    }

    #[test]
    fn render_cache_reuses_an_exact_key_and_invalidates_layout_or_preview_changes() {
        let cache = WaveformRenderCache::default();
        let first = WaveformCacheKey {
            generation: 7,
            bounds: test_bounds(320.0, 180.0),
            peak_scale_bits: 1.0_f32.to_bits(),
        };
        let bands = vec![[0.5, 0.4, 0.3, 0.6]; 2_048];
        let initial_paths = cache.paths_for(7, &bands, 1.0, first.bounds);
        assert_eq!(initial_paths.len(), 4);
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.paths_for(7, &bands, 1.0, first.bounds).len(), 4);
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.paths_for(8, &bands, 1.0, first.bounds).len(), 4);
        assert_eq!(cache.rebuild_count(), 2);
        assert_eq!(
            cache
                .paths_for(8, &bands, 1.0, test_bounds(321.0, 180.0))
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 3);
        assert_eq!(
            cache
                .paths_for(8, &bands, 1.1, test_bounds(321.0, 180.0))
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 4);
    }

    #[test]
    #[ignore = "manual waveform geometry cache benchmark"]
    fn waveform_geometry_cache_benchmark() {
        use std::time::Instant;

        let cache = WaveformRenderCache::default();
        let bands = vec![[0.5, 0.4, 0.3, 0.6]; 2_048];
        let bounds = test_bounds(640.0, 180.0);
        let cold_start = Instant::now();
        for generation in 0..100 {
            let _ = cache.paths_for(generation, &bands, 1.0, bounds);
        }
        let cold = cold_start.elapsed();
        let warm_start = Instant::now();
        for _ in 0..100 {
            let _ = cache.paths_for(99, &bands, 1.0, bounds);
        }
        let warm = warm_start.elapsed();
        eprintln!("waveform geometry: 100 misses {cold:?}; 100 hits {warm:?}");
    }
}
