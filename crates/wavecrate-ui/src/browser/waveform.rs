#![deny(missing_docs)]
#![deny(warnings)]

//! Waveform shaping and canvas rendering for the browser preview panel.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    Bounds, Context, DragMoveEvent, MouseButton, MouseDownEvent, MouseUpEvent, Path, PathBuilder,
    Pixels, Point, Render, Window, canvas, div, fill, point, prelude::*, px,
};
use wavecrate_design::Theme;

use crate::theme::color;

use super::Browser;

const WAVEFORM_SELECTION_CLICK_SLOP_PX: f32 = 2.0;
const WAVEFORM_SELECTION_EDGE_WIDTH: f32 = 1.0;

/// One normalized, ordered playback region in the selected waveform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformPlayRegion {
    pub(super) start: f64,
    pub(super) end: f64,
}

impl WaveformPlayRegion {
    pub(super) fn new(start: f64, end: f64) -> Option<Self> {
        let start = normalized_waveform_position(start)?;
        let end = normalized_waveform_position(end)?;
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        (start < end).then_some(Self { start, end })
    }

    pub(super) fn is_valid(self) -> bool {
        self.start.is_finite()
            && self.end.is_finite()
            && (0.0..=1.0).contains(&self.start)
            && (0.0..=1.0).contains(&self.end)
            && self.start < self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformRegionDrag {
    anchor: f64,
    current: f64,
    anchor_x: f32,
    bounds: Bounds<Pixels>,
    moved: bool,
    generation: u64,
}

impl WaveformRegionDrag {
    fn begin(position: Point<Pixels>, ratio: f64, bounds: Bounds<Pixels>, generation: u64) -> Self {
        Self {
            anchor: ratio,
            current: ratio,
            anchor_x: f32::from(position.x),
            bounds,
            moved: false,
            generation,
        }
    }

    fn update(&mut self, position: Point<Pixels>) {
        if let Some(ratio) = waveform_click_ratio(position, self.bounds) {
            self.current = ratio;
        }
        self.moved |=
            (f32::from(position.x) - self.anchor_x).abs() > WAVEFORM_SELECTION_CLICK_SLOP_PX;
    }

    fn preview_region(self) -> Option<WaveformPlayRegion> {
        WaveformPlayRegion::new(self.anchor, self.current)
    }
}

#[derive(Clone, Copy, Debug)]
struct WaveformDragToken;

struct WaveformDragGhost;

impl Render for WaveformDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

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
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let click_bounds = Rc::new(RefCell::new(None));
    let mut bars = div()
        .id("waveform")
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
    let click_bounds_for_surface = Rc::clone(&click_bounds);
    bars = bars.child(
        canvas(
            move |bounds, _, _| {
                *click_bounds_for_surface.borrow_mut() = Some(bounds);
            },
            move |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0(),
    );
    if let Some(region) = browser
        .waveform_region_drag
        .and_then(WaveformRegionDrag::preview_region)
        .or(browser.waveform_play_region)
    {
        bars = bars.child(waveform_region_overlay(
            region,
            color(theme.colors.selection_fill),
            color(theme.colors.waveform_playhead),
        ));
    }
    if let Some(ratio) = playback_cursor_ratio {
        bars = bars.child(playback_cursor_overlay(
            ratio,
            color(theme.colors.waveform_playhead),
        ));
    }
    let click_bounds_for_down = Rc::clone(&click_bounds);
    bars.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
            window.focus(&view.focus_handle);
            if view.waveform.is_none() {
                return;
            }
            let Some(bounds) = click_bounds_for_down.borrow().as_ref().copied() else {
                return;
            };
            let Some(ratio) = waveform_click_ratio(event.position, bounds) else {
                return;
            };
            view.waveform_region_drag = Some(WaveformRegionDrag::begin(
                event.position,
                ratio,
                bounds,
                view.waveform_generation,
            ));
            cx.notify();
        }),
    )
    .on_drag(WaveformDragToken, |_, _, _, cx| {
        cx.new(|_| WaveformDragGhost)
    })
    .on_drag_move::<WaveformDragToken>(cx.listener(
        |view, event: &DragMoveEvent<WaveformDragToken>, _, cx| {
            view.update_waveform_region_drag(event.event.position);
            cx.notify();
        },
    ))
    .on_mouse_up(
        MouseButton::Left,
        cx.listener(|view, event: &MouseUpEvent, _, cx| {
            view.finish_waveform_region_drag(event.position, cx);
        }),
    )
    .on_mouse_up_out(
        MouseButton::Left,
        cx.listener(|view, event: &MouseUpEvent, _, cx| {
            view.finish_waveform_region_drag(event.position, cx);
        }),
    )
}

impl Browser {
    pub(super) fn clear_waveform_region(&mut self) {
        self.waveform_play_region = None;
        self.waveform_region_drag = None;
    }

    pub(super) fn cancel_waveform_region_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.waveform_region_drag.take().is_none() {
            return false;
        }
        cx.stop_active_drag(window);
        true
    }

    pub(super) fn invalidate_waveform_generation(&mut self) {
        self.waveform_generation = self.waveform_generation.wrapping_add(1);
        self.clear_waveform_region();
    }

    fn update_waveform_region_drag(&mut self, position: Point<Pixels>) {
        let Some(drag) = self.waveform_region_drag.as_mut() else {
            return;
        };
        if !waveform_drag_is_current(*drag, self.waveform_generation, self.waveform.is_some()) {
            self.waveform_region_drag = None;
            return;
        }
        drag.update(position);
    }

    fn finish_waveform_region_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.waveform_region_drag.take() else {
            return;
        };
        if !waveform_drag_is_current(drag, self.waveform_generation, self.waveform.is_some()) {
            return;
        }
        let Some(ratio) = waveform_click_ratio(position, drag.bounds) else {
            return;
        };
        let mut drag = drag;
        drag.update(position);
        if drag.moved {
            self.stop_playback();
            self.waveform_play_region = drag.preview_region();
        } else {
            self.waveform_play_region = None;
            self.start_playback_from_position(ratio, cx);
        }
        cx.notify();
    }
}

fn waveform_click_ratio(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<f64> {
    let width = f32::from(bounds.size.width);
    let left = f32::from(bounds.origin.x);
    let x = f32::from(position.x);
    if !width.is_finite() || width <= 0.0 || !left.is_finite() || !x.is_finite() {
        return None;
    }
    Some(((x - left) / width).clamp(0.0, 1.0) as f64)
}

fn normalized_waveform_position(position: f64) -> Option<f64> {
    position.is_finite().then(|| position.clamp(0.0, 1.0))
}

fn waveform_drag_is_current(
    drag: WaveformRegionDrag,
    generation: u64,
    waveform_loaded: bool,
) -> bool {
    waveform_loaded && drag.generation == generation
}

fn waveform_region_overlay(
    region: WaveformPlayRegion,
    selection_color: gpui::Hsla,
    edge_color: gpui::Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let start = width * region.start as f32;
            let end = width * region.end as f32;
            if !start.is_finite() || !end.is_finite() || end <= start {
                return;
            }
            let selection_bounds = Bounds::from_corners(
                point(bounds.left() + px(start), bounds.top()),
                point(bounds.left() + px(end), bounds.bottom()),
            );
            window.paint_quad(fill(selection_bounds, selection_color));
            let edge_width = WAVEFORM_SELECTION_EDGE_WIDTH.min(width);
            for x in [start, (end - edge_width).max(start)] {
                let edge_bounds = Bounds::from_corners(
                    point(bounds.left() + px(x), bounds.top()),
                    point(
                        bounds.left() + px((x + edge_width).min(width)),
                        bounds.bottom(),
                    ),
                );
                window.paint_quad(fill(edge_bounds, edge_color));
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
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
    fn waveform_click_position_clamps_to_normalized_edges() {
        let bounds = test_bounds(100.0, 20.0);

        assert_eq!(
            waveform_click_ratio(point(px(-10.0), px(5.0)), bounds),
            Some(0.0)
        );
        assert_eq!(
            waveform_click_ratio(point(px(50.0), px(5.0)), bounds),
            Some(0.5)
        );
        assert_eq!(
            waveform_click_ratio(point(px(110.0), px(5.0)), bounds),
            Some(1.0)
        );
    }

    #[test]
    fn waveform_click_position_rejects_empty_bounds() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(0.0), px(20.0)));
        assert_eq!(waveform_click_ratio(point(px(5.0), px(5.0)), bounds), None);
    }

    #[test]
    fn play_region_orders_clamps_and_rejects_zero_width_or_non_finite_ranges() {
        assert_eq!(
            WaveformPlayRegion::new(0.75, 0.25),
            Some(WaveformPlayRegion {
                start: 0.25,
                end: 0.75,
            })
        );
        assert_eq!(
            WaveformPlayRegion::new(-0.2, 1.2),
            Some(WaveformPlayRegion {
                start: 0.0,
                end: 1.0,
            })
        );
        assert_eq!(WaveformPlayRegion::new(0.5, 0.5), None);
        assert_eq!(WaveformPlayRegion::new(f64::NAN, 0.5), None);
        assert_eq!(WaveformPlayRegion::new(0.5, f64::INFINITY), None);
    }

    #[test]
    fn region_drag_uses_logical_pixel_slop_and_clamped_release_position() {
        let bounds = test_bounds(100.0, 20.0);
        let mut drag = WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, 9);

        drag.update(point(px(27.0), px(5.0)));
        assert!(!drag.moved);
        let preview = drag.preview_region().expect("preview remains non-zero");
        assert_eq!(preview.start, 0.25);
        assert!((preview.end - 0.27).abs() < 1e-6);

        let mut vertical_drag =
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, 9);
        vertical_drag.update(point(px(25.0), px(80.0)));
        assert!(!vertical_drag.moved);
        assert_eq!(vertical_drag.preview_region(), None);

        drag.update(point(px(140.0), px(5.0)));
        assert!(drag.moved);
        assert_eq!(
            drag.preview_region(),
            Some(WaveformPlayRegion {
                start: 0.25,
                end: 1.0,
            })
        );
        assert_eq!(drag.generation, 9);

        let mut threshold_drag =
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, 9);
        threshold_drag.update(point(px(27.01), px(5.0)));
        assert!(threshold_drag.moved);

        let mut outside_drag = WaveformRegionDrag::begin(point(px(50.0), px(5.0)), 0.5, bounds, 9);
        outside_drag.update(point(px(-5.0), px(5.0)));
        assert!(outside_drag.moved);
        assert_eq!(outside_drag.current, 0.0);
    }

    #[test]
    fn stale_or_unloaded_region_drags_cannot_commit() {
        let drag =
            WaveformRegionDrag::begin(point(px(20.0), px(5.0)), 0.2, test_bounds(100.0, 20.0), 9);

        assert!(waveform_drag_is_current(drag, 9, true));
        assert!(!waveform_drag_is_current(drag, 10, true));
        assert!(!waveform_drag_is_current(drag, 9, false));
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
