#![deny(missing_docs)]
#![deny(warnings)]

//! Waveform shaping and canvas rendering for the browser preview panel.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{
    AnyView, App, Bounds, Context, DragMoveEvent, Hsla, MouseButton, MouseDownEvent, MouseUpEvent,
    Path, PathBuilder, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, Window, canvas, div,
    fill, point, prelude::*, px,
};
use wavecrate_core::{WaveformRegion, WaveformViewport, snap_resize_ratio_to_whole_bpm};
use wavecrate_design::Theme;

use crate::theme::color;

use super::Browser;
use super::waveform_scrollbar;

const WAVEFORM_SELECTION_CLICK_SLOP_PX: f32 = 2.0;
const WAVEFORM_SELECTION_EDGE_WIDTH: f32 = 1.0;
const PLAY_REGION_RESIZE_HANDLE_HEIGHT: f32 = 22.0;
const PLAY_REGION_RESIZE_HANDLE_WIDTH: f32 = 9.0;
const PLAY_REGION_RESIZE_HIT_WIDTH: f32 = 15.0;
const PLAY_REGION_MOVE_GRIP_HEIGHT: f32 = 7.0;
const PLAY_REGION_MOVE_GRIP_INSET: f32 = 9.0;
const WAVEFORM_PAN_REFERENCE_WIDTH: f64 = 1_200.0;
const WAVEFORM_WHEEL_EPSILON: f64 = f64::EPSILON;
const BPM_SNAP_ICON_SIZE: f32 = 16.0;

/// The transient BPM snap settings used by one waveform resize gesture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BpmSnapSettings {
    /// Whether resize snapping is active.
    pub(super) enabled: bool,
    /// Number of beats represented by the snapped span.
    pub(super) beat_count: u8,
    /// Duration of the currently loaded audio file in seconds.
    pub(super) audio_duration_seconds: f32,
    /// Whether snapping must keep the resulting edge inside the audio file.
    pub(super) constrain_to_audio: bool,
}

impl BpmSnapSettings {
    fn disabled() -> Self {
        Self {
            enabled: false,
            beat_count: 0,
            audio_duration_seconds: 0.0,
            constrain_to_audio: true,
        }
    }
}

fn bpm_snap_icon_point(bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let scale = (width / BPM_SNAP_ICON_SIZE)
        .min(height / BPM_SNAP_ICON_SIZE)
        .max(0.0);
    let left = f32::from(bounds.origin.x) + (width - BPM_SNAP_ICON_SIZE * scale) / 2.0;
    let top = f32::from(bounds.origin.y) + (height - BPM_SNAP_ICON_SIZE * scale) / 2.0;
    point(px(left + x * scale), px(top + y * scale))
}

/// Render the fixed 16×16 BPM snap glyph used by the compact toolbar control.
pub(super) fn render_bpm_snap_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let rail_width = 1.5;
            for left in [1.5, 13.0] {
                let rail = Bounds::from_corners(
                    bpm_snap_icon_point(bounds, left, 2.5),
                    bpm_snap_icon_point(bounds, left + rail_width, 13.5),
                );
                window.paint_quad(fill(rail, icon_color));
            }

            let mut glyph = PathBuilder::fill();
            glyph.move_to(bpm_snap_icon_point(bounds, 5.0, 4.0));
            glyph.line_to(bpm_snap_icon_point(bounds, 8.0, 4.0));
            glyph.cubic_bezier_to(
                bpm_snap_icon_point(bounds, 9.8, 4.0),
                bpm_snap_icon_point(bounds, 10.7, 4.8),
                bpm_snap_icon_point(bounds, 10.7, 6.1),
            );
            glyph.cubic_bezier_to(
                bpm_snap_icon_point(bounds, 10.7, 6.9),
                bpm_snap_icon_point(bounds, 10.2, 7.4),
                bpm_snap_icon_point(bounds, 9.4, 7.7),
            );
            glyph.cubic_bezier_to(
                bpm_snap_icon_point(bounds, 10.4, 8.0),
                bpm_snap_icon_point(bounds, 11.0, 8.8),
                bpm_snap_icon_point(bounds, 11.0, 9.9),
            );
            glyph.cubic_bezier_to(
                bpm_snap_icon_point(bounds, 11.0, 11.4),
                bpm_snap_icon_point(bounds, 9.9, 12.0),
                bpm_snap_icon_point(bounds, 8.2, 12.0),
            );
            glyph.line_to(bpm_snap_icon_point(bounds, 5.0, 12.0));
            glyph.close();
            if let Ok(path) = glyph.build() {
                window.paint_path(path, icon_color);
            }
        },
    )
    .w(px(BPM_SNAP_ICON_SIZE))
    .h(px(BPM_SNAP_ICON_SIZE))
}

struct BpmSnapTooltip;

impl Render for BpmSnapTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(crate::theme::color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child(
                "Snap resized play and edit marks to whole BPM values using the beat guide count.",
            )
    }
}

/// Build the BPM snap tooltip view for the compact toolbar control.
pub(super) fn bpm_snap_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| BpmSnapTooltip).into()
}

struct BeatCountTooltip;

impl Render for BeatCountTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(crate::theme::color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child("Beats in the snapped selection.")
    }
}

pub(super) fn beat_count_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| BeatCountTooltip).into()
}

/// One normalized, ordered playback region in the selected waveform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformPlayRegion {
    pub(super) start: f64,
    pub(super) end: f64,
}

impl WaveformPlayRegion {
    pub(super) fn new(start: f64, end: f64) -> Option<Self> {
        WaveformRegion::new(start, end).map(Self::from_core)
    }

    pub(super) fn is_valid(self) -> bool {
        self.start.is_finite()
            && self.end.is_finite()
            && (0.0..=1.0).contains(&self.start)
            && (0.0..=1.0).contains(&self.end)
            && self.start < self.end
    }

    fn from_core(region: WaveformRegion) -> Self {
        Self {
            start: region.start(),
            end: region.end(),
        }
    }

    fn core(self) -> Option<WaveformRegion> {
        WaveformRegion::new(self.start, self.end)
    }

    fn slide_left(self) -> Option<Self> {
        self.core()
            .map(|region| Self::from_core(region.slide_left()))
    }

    fn slide_right(self) -> Option<Self> {
        self.core()
            .map(|region| Self::from_core(region.slide_right()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveformRegionHandle {
    ResizeStart,
    ResizeEnd,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WaveformRegionDragMode {
    Create,
    Move { baseline: WaveformRegion },
    ResizeStart { baseline: WaveformRegion },
    ResizeEnd { baseline: WaveformRegion },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformRegionDrag {
    anchor: f64,
    current: f64,
    anchor_x: f32,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    moved: bool,
    generation: u64,
    mode: WaveformRegionDragMode,
}

impl WaveformRegionDrag {
    fn begin(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        generation: u64,
    ) -> Self {
        Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            WaveformRegionDragMode::Create,
        )
    }

    fn begin_handle(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformPlayRegion,
        handle: WaveformRegionHandle,
        generation: u64,
    ) -> Option<Self> {
        let baseline = region.core()?;
        let mode = match handle {
            WaveformRegionHandle::ResizeStart => WaveformRegionDragMode::ResizeStart { baseline },
            WaveformRegionHandle::ResizeEnd => WaveformRegionDragMode::ResizeEnd { baseline },
            WaveformRegionHandle::Move => WaveformRegionDragMode::Move { baseline },
        };
        Some(Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            mode,
        ))
    }

    fn with_mode(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        generation: u64,
        mode: WaveformRegionDragMode,
    ) -> Self {
        let ratio = viewport.absolute_ratio_from_visible(visible_ratio);
        Self {
            anchor: ratio,
            current: ratio,
            anchor_x: f32::from(position.x),
            bounds,
            viewport,
            moved: false,
            generation,
            mode,
        }
    }

    fn update(&mut self, position: Point<Pixels>) {
        if let Some(ratio) = waveform_click_ratio(position, self.bounds) {
            self.current = self.viewport.absolute_ratio_from_visible(ratio);
        }
        self.moved |=
            (f32::from(position.x) - self.anchor_x).abs() > WAVEFORM_SELECTION_CLICK_SLOP_PX;
    }

    fn preview_region(self) -> Option<WaveformPlayRegion> {
        self.preview_region_with_bpm(BpmSnapSettings::disabled())
    }

    fn preview_region_with_bpm(self, bpm_snap: BpmSnapSettings) -> Option<WaveformPlayRegion> {
        if !self.moved {
            match self.mode {
                WaveformRegionDragMode::Create => {}
                WaveformRegionDragMode::Move { baseline }
                | WaveformRegionDragMode::ResizeStart { baseline }
                | WaveformRegionDragMode::ResizeEnd { baseline } => {
                    return Some(WaveformPlayRegion::from_core(baseline));
                }
            }
        }
        match self.mode {
            WaveformRegionDragMode::Create => WaveformPlayRegion::new(self.anchor, self.current),
            WaveformRegionDragMode::Move { baseline } => baseline
                .moved_by(self.current - self.anchor)
                .map(WaveformPlayRegion::from_core),
            WaveformRegionDragMode::ResizeStart { baseline } => baseline
                .resized_start_to(snapped_resize_edge(baseline.end(), self.current, bpm_snap))
                .map(WaveformPlayRegion::from_core),
            WaveformRegionDragMode::ResizeEnd { baseline } => baseline
                .resized_end_to(snapped_resize_edge(
                    baseline.start(),
                    self.current,
                    bpm_snap,
                ))
                .map(WaveformPlayRegion::from_core),
        }
    }

    fn ratio_at(self, position: Point<Pixels>) -> Option<f64> {
        waveform_click_ratio(position, self.bounds)
            .map(|ratio| self.viewport.absolute_ratio_from_visible(ratio))
    }

    fn is_handle_drag(self) -> bool {
        !matches!(self.mode, WaveformRegionDragMode::Create)
    }
}

fn snapped_resize_edge(fixed_ratio: f64, moving_ratio: f64, bpm_snap: BpmSnapSettings) -> f64 {
    if !bpm_snap.enabled {
        return moving_ratio;
    }
    f64::from(snap_resize_ratio_to_whole_bpm(
        fixed_ratio as f32,
        moving_ratio as f32,
        bpm_snap.audio_duration_seconds,
        bpm_snap.beat_count,
        bpm_snap.constrain_to_audio,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformPanDrag {
    anchor_visible_ratio: f64,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    total_frames: usize,
    generation: u64,
}

impl WaveformPanDrag {
    fn begin(
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        total_frames: usize,
        generation: u64,
    ) -> Option<Self> {
        Some(Self {
            anchor_visible_ratio: waveform_click_ratio(position, bounds)?,
            viewport,
            bounds,
            total_frames,
            generation,
        })
    }

    fn update(self, position: Point<Pixels>) -> Option<WaveformViewport> {
        let current = waveform_click_ratio(position, self.bounds)?;
        Some(self.viewport.pan_by_visible_ratio_drag(
            self.total_frames,
            self.anchor_visible_ratio,
            current,
        ))
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
/// path coordinates or shaped envelope, including the exact source range and refinement revision
/// of a viewport detail projection.
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
    viewport_start_bits: u64,
    viewport_end_bits: u64,
    projection_revision: u64,
    projection_start_bits: u64,
    projection_end_bits: u64,
}

impl WaveformRenderCache {
    /// Return paths for this exact preview generation and layout, rebuilding only on a key miss.
    fn paths_for(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
    ) -> Vec<(usize, Path<Pixels>)> {
        let key = WaveformCacheKey {
            generation,
            bounds,
            peak_scale_bits: peak_scale.to_bits(),
            viewport_start_bits: viewport.start.to_bits(),
            viewport_end_bits: viewport.end.to_bits(),
            projection_revision,
            projection_start_bits: source_range.0.to_bits(),
            projection_end_bits: source_range.1.to_bits(),
        };
        let mut state = self.state.borrow_mut();
        if state.key != Some(key) {
            state.paths = build_waveform_paths(bands, peak_scale, bounds, viewport, source_range);
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
    viewport: WaveformViewport,
    source_range: (f64, f64),
) -> Vec<(usize, Path<Pixels>)> {
    let point_count = drawable_point_count(bounds.size.width, bands.len());
    let extents = resample_extents(bands, point_count, peak_scale, viewport, source_range);
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
fn resample_extents(
    bands: &[[f32; 4]],
    point_count: usize,
    peak_scale: f32,
    viewport: WaveformViewport,
    source_range: (f64, f64),
) -> Vec<[f32; 4]> {
    if bands.is_empty() || point_count == 0 {
        return Vec::new();
    }
    let point_count = point_count.min(bands.len());
    let mut extents = vec![[0.0_f32; 4]; point_count];
    let viewport_start = viewport.start;
    let viewport_width = viewport.width();
    let band_count = bands.len() as f64;
    let source_start = source_range.0;
    let source_width = source_range.1 - source_start;
    if !source_start.is_finite() || !source_width.is_finite() || source_width <= 0.0 {
        return extents;
    }
    for (index, column) in bands.iter().copied().enumerate() {
        let band_start = source_start + index as f64 / band_count * source_width;
        let band_end = source_start + (index + 1) as f64 / band_count * source_width;
        let overlap_start = band_start.max(viewport_start);
        let overlap_end = band_end.min(viewport_start + viewport_width);
        if overlap_end <= overlap_start {
            continue;
        }
        let first_bucket = (((overlap_start - viewport_start) / viewport_width)
            * point_count as f64)
            .floor()
            .clamp(0.0, (point_count - 1) as f64) as usize;
        let last_bucket = ((((overlap_end - viewport_start) / viewport_width) * point_count as f64)
            .ceil()
            .max((first_bucket + 1) as f64)
            .min(point_count as f64)) as usize;
        let values = [
            waveform_extent(column, 0, peak_scale),
            waveform_extent(column, 1, peak_scale),
            waveform_extent(column, 2, peak_scale),
            waveform_extent(column, 3, peak_scale),
        ];
        for bucket in first_bucket..last_bucket {
            for (lane, value) in values.into_iter().enumerate() {
                extents[bucket][lane] = extents[bucket][lane].max(value);
            }
        }
    }
    extents
}

fn visible_peak_values(peaks: &[f32], viewport: WaveformViewport) -> Vec<f32> {
    if peaks.is_empty() {
        return Vec::new();
    }
    let point_count =
        ((peaks.len() as f64 * viewport.width()).ceil() as usize).clamp(1, peaks.len());
    let mut visible = vec![0.0_f32; point_count];
    let peak_count = peaks.len() as f64;
    for (index, peak) in peaks.iter().copied().enumerate() {
        let peak_start = index as f64 / peak_count;
        let peak_end = (index + 1) as f64 / peak_count;
        let overlap_start = peak_start.max(viewport.start);
        let overlap_end = peak_end.min(viewport.end);
        if overlap_end <= overlap_start {
            continue;
        }
        let first_bucket = (((overlap_start - viewport.start) / viewport.width())
            * point_count as f64)
            .floor()
            .clamp(0.0, (point_count - 1) as f64) as usize;
        let last_bucket = ((((overlap_end - viewport.start) / viewport.width())
            * point_count as f64)
            .ceil()
            .max((first_bucket + 1) as f64)
            .min(point_count as f64)) as usize;
        for bucket in first_bucket..last_bucket {
            visible[bucket] = visible[bucket].max(peak);
        }
    }
    visible
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
        .flex_1()
        .w_full()
        .overflow_hidden()
        .relative()
        .bg(color(theme.colors.waveform_background));
    if let Some(preview) = browser.waveform.as_ref() {
        let viewport = browser.waveform_viewport;
        let generation = browser.waveform_generation;
        browser
            .waveform_detail
            .schedule_for_preview(preview, viewport, generation, cx);
        if !preview.bands.is_empty() {
            let projection = browser
                .waveform_detail
                .projection_for(preview, viewport, generation);
            let (bands, source_range, projection_revision): (Arc<[[f32; 4]]>, (f64, f64), u64) =
                projection
                    .map(|projection| {
                        (
                            projection.bands,
                            projection.source_range,
                            projection.projection_revision,
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            Arc::from(preview.bands.clone().into_boxed_slice()),
                            (0.0, 1.0),
                            0,
                        )
                    });
            let lane_colors = [
                color(theme.colors.waveform_low),
                color(theme.colors.waveform_mid),
                color(theme.colors.waveform_high),
                color(theme.colors.waveform_raw),
            ];
            let peak_scale = theme.waveform.peak_scale;
            let cache = browser.waveform_render_cache.clone();
            let waveform = canvas(
                move |_, _, _| (),
                move |bounds, _, window, _| {
                    for (lane, path) in cache
                        .paths_for(
                            generation,
                            &bands,
                            peak_scale,
                            bounds,
                            viewport,
                            source_range,
                            projection_revision,
                        )
                        .into_iter()
                    {
                        window.paint_path(path, lane_colors[lane]);
                    }
                },
            )
            .size_full();
            bars = bars.child(waveform);
        } else {
            let scrollbar_height = if viewport.scrollbar_visible() {
                theme.waveform.scrollbar_height
            } else {
                0.0
            };
            let available_height = (theme.spacing.waveform_height
                - theme.waveform.title_height
                - theme.waveform.metadata_height
                - scrollbar_height)
                .max(theme.spacing.waveform_min_bar_height);
            let bar_color = if browser.playing_path.is_some() {
                theme.colors.waveform_played
            } else {
                theme.colors.waveform_unplayed
            };
            for peak in visible_peak_values(&preview.peaks, viewport) {
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
    let detail_state_for_surface = browser.waveform_detail.clone();
    let detail_active_for_surface = browser.waveform.is_some();
    bars = bars.child(
        canvas(
            move |bounds, window, _| {
                *click_bounds_for_surface.borrow_mut() = Some(bounds);
                if detail_state_for_surface.note_layout_width(f32::from(bounds.size.width))
                    && detail_active_for_surface
                {
                    // The canvas is the first place where the final layout width is known. A
                    // single follow-up frame lets the scheduler observe a resize without adding
                    // an idle animation loop.
                    //
                    // `Canvas`' prepaint callback has a window but no entity context.
                    // `request_animation_frame` is therefore the smallest event-driven bridge to
                    // the next render, where `schedule_for_preview` owns the debounce/task.
                    // The follow-up request is only made when the measured width changes.
                    window.request_animation_frame();
                }
            },
            move |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0(),
    );
    let bpm_snap = browser.bpm_snap_settings();
    if let Some(region) = browser
        .waveform_region_drag
        .and_then(|drag| drag.preview_region_with_bpm(bpm_snap))
        .or(browser.waveform_play_region)
    {
        bars = bars.child(waveform_region_overlay(
            region,
            browser.waveform_viewport,
            color(theme.colors.selection_fill),
            color(theme.colors.waveform_playhead),
        ));
    }
    if let Some(ratio) = playback_cursor_ratio.and_then(|ratio| {
        browser
            .waveform_viewport
            .visible_ratio_from_absolute(f64::from(ratio))
            .map(|ratio| ratio as f32)
    }) {
        bars = bars.child(playback_cursor_overlay(
            ratio,
            color(theme.colors.waveform_playhead),
        ));
    }
    let click_bounds_for_down = Rc::clone(&click_bounds);
    let click_bounds_for_wheel = Rc::clone(&click_bounds);
    let click_bounds_for_middle = Rc::clone(&click_bounds);
    let bars = bars
        .track_focus(&browser.waveform_focus_handle)
        .on_scroll_wheel(
            cx.listener(move |view, event: &ScrollWheelEvent, window, cx| {
                let bounds = click_bounds_for_wheel.borrow().as_ref().copied();
                window.focus(&view.waveform_focus_handle);
                if view.handle_waveform_wheel(event, bounds) {
                    cx.notify();
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                let Some(bounds) = click_bounds_for_middle.borrow().as_ref().copied() else {
                    return;
                };
                if !bounds.contains(&event.position) {
                    return;
                }
                view.begin_waveform_pan(event.position, bounds, window, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                window.focus(&view.waveform_focus_handle);
                if view.waveform.is_none() {
                    return;
                }
                let Some(bounds) = click_bounds_for_down.borrow().as_ref().copied() else {
                    return;
                };
                if !bounds.contains(&event.position) {
                    return;
                }
                let Some(ratio) = waveform_click_ratio(event.position, bounds) else {
                    return;
                };
                view.waveform_zoom_anchor_ratio =
                    view.waveform_viewport.absolute_ratio_from_visible(ratio);
                view.waveform_region_drag = view
                    .waveform_play_region
                    .and_then(|region| {
                        waveform_region_handle_at(
                            region,
                            view.waveform_viewport,
                            bounds,
                            event.position,
                        )
                    })
                    .and_then(|handle| {
                        view.waveform_play_region.and_then(|region| {
                            WaveformRegionDrag::begin_handle(
                                event.position,
                                ratio,
                                bounds,
                                view.waveform_viewport,
                                region,
                                handle,
                                view.waveform_generation,
                            )
                        })
                    })
                    .or_else(|| {
                        Some(WaveformRegionDrag::begin(
                            event.position,
                            ratio,
                            bounds,
                            view.waveform_viewport,
                            view.waveform_generation,
                        ))
                    });
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
        );
    let scrollbar = waveform_scrollbar::render(browser, theme.clone(), cx);
    div()
        .id("waveform-area")
        .flex()
        .flex_col()
        .flex_1()
        .w_full()
        .child(bars)
        .child(scrollbar)
}

impl Browser {
    pub(super) fn clear_waveform_region(&mut self) {
        self.waveform_play_region = None;
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        self.waveform_scrollbar_drag = None;
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

    pub(super) fn cancel_waveform_gestures(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let had_region_drag = self.waveform_region_drag.take().is_some();
        let had_pan_drag = self.waveform_pan_drag.take().is_some();
        let had_scrollbar_drag = self.waveform_scrollbar_drag.take().is_some();
        let canceled = had_region_drag || had_pan_drag || had_scrollbar_drag;
        if canceled {
            cx.stop_active_drag(window);
        }
        canceled
    }

    pub(super) fn invalidate_waveform_generation(&mut self) {
        self.waveform_generation = self.waveform_generation.wrapping_add(1);
        self.waveform_detail.reset();
        self.waveform_viewport = WaveformViewport::full();
        self.waveform_zoom_anchor_ratio = 0.5;
        self.clear_waveform_region();
    }

    fn begin_waveform_pan(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.waveform.as_ref() else {
            return;
        };
        window.focus(&self.waveform_focus_handle);
        if let Some(visible_ratio) = waveform_click_ratio(position, bounds) {
            self.waveform_zoom_anchor_ratio = self
                .waveform_viewport
                .absolute_ratio_from_visible(visible_ratio);
        }
        self.waveform_pan_drag = WaveformPanDrag::begin(
            position,
            bounds,
            self.waveform_viewport,
            preview.total_frames,
            self.waveform_generation,
        );
    }

    pub(super) fn update_waveform_pan_drag(
        &mut self,
        position: Point<Pixels>,
        middle_button_pressed: bool,
    ) -> bool {
        if !middle_button_pressed {
            return false;
        }
        let Some(drag) = self.waveform_pan_drag else {
            return false;
        };
        if drag.generation != self.waveform_generation || self.waveform.is_none() {
            self.waveform_pan_drag = None;
            return false;
        }
        let Some(viewport) = drag.update(position) else {
            return false;
        };
        if self.waveform_viewport.is_effectively_equal(viewport) {
            return false;
        }
        self.waveform_viewport = viewport;
        true
    }

    pub(super) fn finish_waveform_pan_drag(&mut self) -> bool {
        self.waveform_pan_drag.take().is_some()
    }

    fn handle_waveform_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        bounds: Option<Bounds<Pixels>>,
    ) -> bool {
        let Some(preview) = self.waveform.as_ref() else {
            return false;
        };
        let Some(bounds) = bounds else {
            return false;
        };
        let Some(anchor_ratio) = waveform_click_ratio(event.position, bounds) else {
            return false;
        };
        let Some(action) = waveform_wheel_action(event.delta, event.modifiers.shift) else {
            return false;
        };
        self.waveform_zoom_anchor_ratio = self
            .waveform_viewport
            .absolute_ratio_from_visible(anchor_ratio);
        let next = match action {
            WaveformWheelAction::Pan(fraction) => self
                .waveform_viewport
                .pan_by_visible_fraction(preview.total_frames, fraction),
            WaveformWheelAction::ZoomIn => self.waveform_viewport.zoom_around_visible_anchor(
                preview.total_frames,
                anchor_ratio,
                true,
                true,
            ),
            WaveformWheelAction::ZoomOut {
                expand_silence_margin,
            } => self.waveform_viewport.zoom_around_visible_anchor(
                preview.total_frames,
                anchor_ratio,
                false,
                expand_silence_margin,
            ),
        };
        if self.waveform_viewport.is_effectively_equal(next) {
            return false;
        }
        self.waveform_viewport = next;
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        true
    }

    pub(super) fn zoom_waveform_to_selection(&mut self) -> bool {
        let Some(preview) = self.waveform.as_ref() else {
            return false;
        };
        let next = if let Some(region) =
            self.waveform_play_region.filter(|region| region.is_valid())
        {
            self.waveform_viewport
                .zoom_to_region(preview.total_frames, region.start, region.end)
        } else {
            let anchor = self
                .playback_cursor_sample
                .map(|sample| f64::from(sample.ratio))
                .filter(|ratio| {
                    ratio.is_finite()
                        && self
                            .waveform_viewport
                            .visible_ratio_from_absolute(*ratio)
                            .is_some()
                })
                .unwrap_or(self.waveform_zoom_anchor_ratio);
            self.waveform_viewport
                .zoom_around_absolute_anchor(preview.total_frames, anchor)
        };
        if self.waveform_viewport.is_effectively_equal(next) {
            return false;
        }
        self.waveform_viewport = next;
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        true
    }

    pub(super) fn reset_waveform_viewport(&mut self) -> bool {
        if self.waveform.is_none() {
            return false;
        }
        let next = WaveformViewport::full();
        if self.waveform_viewport.is_effectively_equal(next) {
            return false;
        }
        self.waveform_viewport = next;
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        true
    }

    pub(super) fn slide_waveform_region(&mut self, direction: WaveformRegionSlide) -> bool {
        let Some(region) = self.waveform_play_region else {
            return false;
        };
        let Some(next) = (match direction {
            WaveformRegionSlide::Left => region.slide_left(),
            WaveformRegionSlide::Right => region.slide_right(),
        }) else {
            return false;
        };
        if next == region {
            return false;
        }
        self.waveform_play_region = Some(next);
        let Some(preview) = self.waveform.as_ref() else {
            return true;
        };
        let viewport = self.waveform_viewport;
        let viewport_shift = if next.start < viewport.start {
            next.start - viewport.start
        } else if next.end > viewport.end {
            next.end - viewport.end
        } else {
            0.0
        };
        if viewport_shift.abs() > WAVEFORM_WHEEL_EPSILON {
            self.waveform_viewport = viewport
                .pan_by_visible_fraction(preview.total_frames, viewport_shift / viewport.width());
        }
        true
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
        let mut drag = drag;
        drag.update(position);
        let bpm_snap = self.bpm_snap_settings();
        if drag.is_handle_drag() {
            if drag.moved {
                if let Some(region) = drag.preview_region_with_bpm(bpm_snap) {
                    self.waveform_play_region = Some(region);
                }
            }
        } else if drag.moved {
            self.stop_playback();
            self.waveform_play_region = drag.preview_region();
        } else if let Some(ratio) = drag.ratio_at(position) {
            self.waveform_play_region = None;
            self.start_playback_from_position(ratio, cx);
        }
        cx.notify();
    }
}

/// Translate GPUI's native scroll delta into waveform gesture coordinates.
///
/// GPUI preserves the platform's macOS content-direction delta. Convert both axes to the
/// reference's logical scroll-offset direction here: positive offset movement reveals later
/// content, and the viewport therefore advances toward a positive normalized position.
fn scroll_delta(delta: ScrollDelta) -> (f64, f64) {
    match delta {
        ScrollDelta::Pixels(delta) => (-f64::from(delta.x), -f64::from(delta.y)),
        ScrollDelta::Lines(delta) => (-f64::from(delta.x), -f64::from(delta.y)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WaveformWheelAction {
    Pan(f64),
    ZoomIn,
    ZoomOut { expand_silence_margin: bool },
}

fn waveform_wheel_action(delta: ScrollDelta, shift: bool) -> Option<WaveformWheelAction> {
    let (delta_x, delta_y) = scroll_delta(delta);
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return None;
    }
    if delta_x.abs() > delta_y.abs() && delta_x.abs() > WAVEFORM_WHEEL_EPSILON {
        Some(WaveformWheelAction::Pan(
            delta_x / WAVEFORM_PAN_REFERENCE_WIDTH,
        ))
    } else if delta_y < -WAVEFORM_WHEEL_EPSILON {
        Some(WaveformWheelAction::ZoomIn)
    } else if delta_y > WAVEFORM_WHEEL_EPSILON {
        Some(WaveformWheelAction::ZoomOut {
            expand_silence_margin: shift,
        })
    } else {
        None
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

fn waveform_region_handle_at(
    region: WaveformPlayRegion,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<WaveformRegionHandle> {
    if !bounds.contains(&position) {
        return None;
    }
    let width = f32::from(bounds.size.width);
    let left = f32::from(bounds.origin.x);
    let x = f32::from(position.x);
    let y = f32::from(position.y);
    let top = f32::from(bounds.origin.y);
    if !width.is_finite()
        || width <= 0.0
        || !left.is_finite()
        || !x.is_finite()
        || !y.is_finite()
        || !top.is_finite()
    {
        return None;
    }
    let in_resize_strip = y >= top && y <= top + PLAY_REGION_RESIZE_HANDLE_HEIGHT;
    if in_resize_strip {
        let start_x = viewport
            .visible_ratio_from_absolute(region.start)
            .map(|ratio| left + width * ratio as f32);
        let end_x = viewport
            .visible_ratio_from_absolute(region.end)
            .map(|ratio| left + width * ratio as f32);
        let half_hit_width = PLAY_REGION_RESIZE_HIT_WIDTH * 0.5;
        let start_distance = start_x
            .filter(|center| (x - center).abs() <= half_hit_width)
            .map(|center| (x - center).abs());
        let end_distance = end_x
            .filter(|center| (x - center).abs() <= half_hit_width)
            .map(|center| (x - center).abs());
        match (start_distance, end_distance) {
            (Some(start), Some(end)) if end < start => {
                return Some(WaveformRegionHandle::ResizeEnd);
            }
            (Some(_), Some(_)) | (Some(_), None) => {
                return Some(WaveformRegionHandle::ResizeStart);
            }
            (None, Some(_)) => return Some(WaveformRegionHandle::ResizeEnd),
            (None, None) => {}
        }
    }

    if y > top + PLAY_REGION_MOVE_GRIP_HEIGHT {
        return None;
    }
    let Some((visible_start, visible_end)) =
        viewport.visible_range_from_absolute(region.start, region.end)
    else {
        return None;
    };
    let grip_left = left + width * visible_start as f32 + PLAY_REGION_MOVE_GRIP_INSET;
    let grip_right = left + width * visible_end as f32 - PLAY_REGION_MOVE_GRIP_INSET;
    (grip_right > grip_left && x >= grip_left && x <= grip_right)
        .then_some(WaveformRegionHandle::Move)
}

fn waveform_drag_is_current(
    drag: WaveformRegionDrag,
    generation: u64,
    waveform_loaded: bool,
) -> bool {
    waveform_loaded && drag.generation == generation
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveformRegionSlide {
    Left,
    Right,
}

pub(super) fn waveform_region_navigation_key(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> Option<WaveformRegionSlide> {
    if secondary || shift || alt || function {
        return None;
    }
    match key {
        "left" | "arrowleft" => Some(WaveformRegionSlide::Left),
        "right" | "arrowright" => Some(WaveformRegionSlide::Right),
        _ => None,
    }
}

fn waveform_region_overlay(
    region: WaveformPlayRegion,
    viewport: WaveformViewport,
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
            let Some((visible_start, visible_end)) =
                viewport.visible_range_from_absolute(region.start, region.end)
            else {
                return;
            };
            let start = width * visible_start as f32;
            let end = width * visible_end as f32;
            if !start.is_finite() || !end.is_finite() || end <= start {
                return;
            }
            let selection_bounds = Bounds::from_corners(
                point(bounds.left() + px(start), bounds.top()),
                point(bounds.left() + px(end), bounds.bottom()),
            );
            window.paint_quad(fill(selection_bounds, selection_color));
            let edge_width = WAVEFORM_SELECTION_EDGE_WIDTH.min(width);
            for absolute in [region.start, region.end] {
                let Some(visible) = viewport.visible_ratio_from_absolute(absolute) else {
                    continue;
                };
                let center = width * visible as f32;
                let left = (center - edge_width * 0.5).clamp(0.0, width - edge_width);
                let edge_bounds = Bounds::from_corners(
                    point(bounds.left() + px(left), bounds.top()),
                    point(bounds.left() + px(left + edge_width), bounds.bottom()),
                );
                window.paint_quad(fill(edge_bounds, edge_color));
            }
            let move_left = viewport
                .visible_ratio_from_absolute(region.start)
                .unwrap_or(visible_start) as f32;
            let move_right = viewport
                .visible_ratio_from_absolute(region.end)
                .unwrap_or(visible_end) as f32;
            let grip_left = width * move_left + PLAY_REGION_MOVE_GRIP_INSET;
            let grip_right = width * move_right - PLAY_REGION_MOVE_GRIP_INSET;
            let grip_height = PLAY_REGION_MOVE_GRIP_HEIGHT.min(height);
            if grip_right > grip_left && grip_height > 0.0 {
                let grip_bounds = Bounds::from_corners(
                    point(bounds.left() + px(grip_left), bounds.top()),
                    point(
                        bounds.left() + px(grip_right),
                        bounds.top() + px(grip_height),
                    ),
                );
                window.paint_quad(fill(grip_bounds, edge_color.opacity(0.75)));
            }
            let handle_height = PLAY_REGION_RESIZE_HANDLE_HEIGHT.min(height);
            if handle_height > 0.0 {
                for absolute in [region.start, region.end] {
                    let Some(visible) = viewport.visible_ratio_from_absolute(absolute) else {
                        continue;
                    };
                    let center = width * visible as f32;
                    let handle_width = PLAY_REGION_RESIZE_HANDLE_WIDTH.min(width);
                    let left = (center - handle_width * 0.5).clamp(0.0, width - handle_width);
                    let handle_bounds = Bounds::from_corners(
                        point(bounds.left() + px(left), bounds.top()),
                        point(
                            bounds.left() + px(left + handle_width),
                            bounds.top() + px(handle_height),
                        ),
                    );
                    window.paint_quad(fill(handle_bounds, edge_color.opacity(0.9)));
                }
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

        let extents = resample_extents(
            &bands,
            drawable_point_count(px(320.0), bands.len()),
            1.0,
            WaveformViewport::full(),
            (0.0, 1.0),
        );

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
    fn resampling_leaves_silence_margin_columns_blank() {
        let bands = vec![[1.0, 1.0, 1.0, 1.0]; 16];
        let viewport = WaveformViewport {
            start: -0.5,
            end: 1.5,
        };
        let extents = resample_extents(&bands, 8, 1.0, viewport, (0.0, 1.0));
        assert!(extents[0].iter().all(|value| *value == 0.0));
        assert!(extents[1].iter().all(|value| *value == 0.0));
        assert!(extents[6].iter().all(|value| *value == 0.0));
        assert!(extents[7].iter().all(|value| *value == 0.0));
        assert!(
            extents[2..6]
                .iter()
                .flat_map(|column| column.iter())
                .any(|value| *value > 0.0)
        );

        let peaks = visible_peak_values(&[1.0; 8], viewport);
        assert!(peaks[0] == 0.0 && peaks[1] == 0.0);
        assert!(peaks[6] == 0.0 && peaks[7] == 0.0);
        assert!(peaks[2..6].iter().any(|peak| *peak > 0.0));
    }

    #[test]
    fn detail_projection_maps_only_its_source_range_into_silence_margins() {
        let bands = vec![[1.0, 1.0, 1.0, 1.0]; 16];
        let extents = resample_extents(
            &bands,
            16,
            1.0,
            WaveformViewport {
                start: -0.5,
                end: 1.5,
            },
            (0.25, 0.5),
        );
        assert!(
            extents[..6]
                .iter()
                .flat_map(|column| column.iter())
                .all(|value| *value == 0.0)
        );
        assert!(
            extents[10..]
                .iter()
                .flat_map(|column| column.iter())
                .all(|value| *value == 0.0)
        );
        assert!(
            extents[6..10]
                .iter()
                .flat_map(|column| column.iter())
                .any(|value| *value > 0.0)
        );
    }

    #[test]
    fn render_cache_reuses_an_exact_key_and_invalidates_layout_or_preview_changes() {
        let cache = WaveformRenderCache::default();
        let viewport = WaveformViewport::full();
        let first = WaveformCacheKey {
            generation: 7,
            bounds: test_bounds(320.0, 180.0),
            peak_scale_bits: 1.0_f32.to_bits(),
            viewport_start_bits: viewport.start.to_bits(),
            viewport_end_bits: viewport.end.to_bits(),
            projection_revision: 0,
            projection_start_bits: 0.0_f64.to_bits(),
            projection_end_bits: 1.0_f64.to_bits(),
        };
        let bands = vec![[0.5, 0.4, 0.3, 0.6]; 2_048];
        let initial_paths = cache.paths_for(7, &bands, 1.0, first.bounds, viewport, (0.0, 1.0), 0);
        assert_eq!(initial_paths.len(), 4);
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(
            cache
                .paths_for(7, &bands, 1.0, first.bounds, viewport, (0.0, 1.0), 0)
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(
            cache
                .paths_for(8, &bands, 1.0, first.bounds, viewport, (0.0, 1.0), 0)
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 2);
        assert_eq!(
            cache
                .paths_for(
                    8,
                    &bands,
                    1.0,
                    test_bounds(321.0, 180.0),
                    viewport,
                    (0.0, 1.0),
                    0,
                )
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 3);
        assert_eq!(
            cache
                .paths_for(
                    8,
                    &bands,
                    1.1,
                    test_bounds(321.0, 180.0),
                    viewport,
                    (0.0, 1.0),
                    0,
                )
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 4);
        let zoomed = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        assert_eq!(
            cache
                .paths_for(
                    8,
                    &bands,
                    1.1,
                    test_bounds(321.0, 180.0),
                    zoomed,
                    (0.0, 1.0),
                    0,
                )
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 5);
        assert_eq!(
            cache
                .paths_for(
                    8,
                    &bands,
                    1.1,
                    test_bounds(321.0, 180.0),
                    zoomed,
                    (0.25, 0.75),
                    1,
                )
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 6);
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
    fn gpui_native_wheel_sign_routes_up_to_zoom_in_and_down_to_zoom_out() {
        assert_eq!(
            waveform_wheel_action(ScrollDelta::Lines(point(0.0, 1.0)), false),
            Some(WaveformWheelAction::ZoomIn)
        );
        assert_eq!(
            waveform_wheel_action(ScrollDelta::Lines(point(0.0, -1.0)), false),
            Some(WaveformWheelAction::ZoomOut {
                expand_silence_margin: false,
            })
        );
        assert_eq!(
            waveform_wheel_action(ScrollDelta::Lines(point(0.0, -1.0)), true),
            Some(WaveformWheelAction::ZoomOut {
                expand_silence_margin: true,
            })
        );
        assert_eq!(
            waveform_wheel_action(ScrollDelta::Lines(point(-1.0, 0.0)), false),
            Some(WaveformWheelAction::Pan(1.0 / WAVEFORM_PAN_REFERENCE_WIDTH))
        );
        assert_eq!(
            waveform_wheel_action(ScrollDelta::Lines(point(1.0, 0.0)), false),
            Some(WaveformWheelAction::Pan(
                -1.0 / WAVEFORM_PAN_REFERENCE_WIDTH
            ))
        );
    }

    #[test]
    fn waveform_click_position_rejects_empty_bounds() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(0.0), px(20.0)));
        assert_eq!(waveform_click_ratio(point(px(5.0), px(5.0)), bounds), None);
    }

    #[test]
    fn region_drag_translates_visible_pointer_to_absolute_file_ratio() {
        let bounds = test_bounds(100.0, 20.0);
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        let mut drag =
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, viewport, 4);
        drag.update(point(px(75.0), px(5.0)));
        assert_eq!(
            drag.preview_region(),
            Some(WaveformPlayRegion {
                start: 0.375,
                end: 0.625,
            })
        );
        assert_eq!(drag.ratio_at(point(px(100.0), px(5.0))), Some(0.75));
    }

    #[test]
    fn middle_pan_drag_uses_captured_viewport_and_generation() {
        let bounds = test_bounds(100.0, 20.0);
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        let drag = WaveformPanDrag::begin(point(px(50.0), px(5.0)), bounds, viewport, 1_000, 12)
            .expect("inside waveform bounds");
        assert_eq!(drag.generation, 12);
        let panned = drag
            .update(point(px(25.0), px(5.0)))
            .expect("inside waveform bounds");
        assert!((panned.start - 0.375).abs() < 1e-12);
        assert!((panned.end - 0.875).abs() < 1e-12);
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
        assert!(
            !WaveformPlayRegion {
                start: -0.1,
                end: 0.5,
            }
            .is_valid()
        );
        assert!(
            !WaveformPlayRegion {
                start: 0.8,
                end: 0.2,
            }
            .is_valid()
        );
    }

    #[test]
    fn region_drag_uses_logical_pixel_slop_and_clamped_release_position() {
        let bounds = test_bounds(100.0, 20.0);
        let viewport = WaveformViewport::full();
        let mut drag =
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, viewport, 9);

        drag.update(point(px(27.0), px(5.0)));
        assert!(!drag.moved);
        let preview = drag.preview_region().expect("preview remains non-zero");
        assert_eq!(preview.start, 0.25);
        assert!((preview.end - 0.27).abs() < 1e-6);

        let mut vertical_drag =
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, viewport, 9);
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
            WaveformRegionDrag::begin(point(px(25.0), px(5.0)), 0.25, bounds, viewport, 9);
        threshold_drag.update(point(px(27.01), px(5.0)));
        assert!(threshold_drag.moved);

        let mut outside_drag =
            WaveformRegionDrag::begin(point(px(50.0), px(5.0)), 0.5, bounds, viewport, 9);
        outside_drag.update(point(px(-5.0), px(5.0)));
        assert!(outside_drag.moved);
        assert_eq!(outside_drag.current, 0.0);
    }

    #[test]
    fn play_region_handle_hit_zones_prioritize_resize_and_respect_grip_geometry() {
        let bounds = test_bounds(200.0, 100.0);
        let region = WaveformPlayRegion::new(0.25, 0.75).unwrap();
        let viewport = WaveformViewport::full();

        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(50.0), px(10.0))),
            Some(WaveformRegionHandle::ResizeStart)
        );
        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(150.0), px(10.0))),
            Some(WaveformRegionHandle::ResizeEnd)
        );
        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(100.0), px(3.0))),
            Some(WaveformRegionHandle::Move)
        );
        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(100.0), px(8.0))),
            None
        );
        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(100.0), px(24.0))),
            None
        );
    }

    #[test]
    fn play_region_handles_follow_zoomed_coordinates_and_hide_offscreen_edges() {
        let bounds = test_bounds(200.0, 100.0);
        let region = WaveformPlayRegion::new(0.4, 0.6).unwrap();
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };

        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(60.0), px(10.0))),
            Some(WaveformRegionHandle::ResizeStart)
        );
        assert_eq!(
            waveform_region_handle_at(region, viewport, bounds, point(px(140.0), px(10.0))),
            Some(WaveformRegionHandle::ResizeEnd)
        );

        let clipped = WaveformPlayRegion::new(0.1, 0.9).unwrap();
        assert_eq!(
            waveform_region_handle_at(clipped, viewport, bounds, point(px(0.0), px(10.0))),
            None
        );
        assert_eq!(
            waveform_region_handle_at(clipped, viewport, bounds, point(px(200.0), px(10.0))),
            None
        );
        assert_eq!(
            waveform_region_handle_at(clipped, viewport, bounds, point(px(100.0), px(3.0))),
            Some(WaveformRegionHandle::Move)
        );
    }

    #[test]
    fn handle_drags_preview_move_and_resize_from_the_original_baseline() {
        let bounds = test_bounds(100.0, 100.0);
        let viewport = WaveformViewport::full();
        let baseline = WaveformPlayRegion::new(0.25, 0.5).unwrap();

        let mut move_drag = WaveformRegionDrag::begin_handle(
            point(px(37.5), px(3.0)),
            0.375,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::Move,
            7,
        )
        .unwrap();
        move_drag.update(point(px(62.5), px(3.0)));
        let moved = move_drag.preview_region().unwrap();
        assert!((moved.start - 0.5).abs() < 1.0e-12);
        assert!((moved.end - 0.75).abs() < 1.0e-12);
        assert!(move_drag.is_handle_drag());

        let mut resize_drag = WaveformRegionDrag::begin_handle(
            point(px(25.0), px(10.0)),
            0.25,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::ResizeStart,
            7,
        )
        .unwrap();
        resize_drag.update(point(px(80.0), px(10.0)));
        let resized = resize_drag.preview_region().unwrap();
        assert!((resized.start - 0.5).abs() < 1.0e-6);
        assert!((resized.end - 0.8).abs() < 1.0e-6);
        assert!(resize_drag.moved);

        let mut no_op = WaveformRegionDrag::begin_handle(
            point(px(27.0), px(10.0)),
            0.27,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::ResizeStart,
            7,
        )
        .unwrap();
        assert_eq!(no_op.preview_region(), Some(baseline));
        no_op.update(point(px(28.0), px(10.0)));
        assert!(!no_op.moved);
        assert_eq!(no_op.preview_region(), Some(baseline));
    }

    #[test]
    fn bpm_snap_resize_uses_a_whole_bpm_result() {
        let bounds = test_bounds(100.0, 100.0);
        let viewport = WaveformViewport::full();
        let baseline = WaveformPlayRegion::new(0.2, 0.4).unwrap();
        let bpm_snap = BpmSnapSettings {
            enabled: true,
            beat_count: 4,
            audio_duration_seconds: 10.0,
            constrain_to_audio: true,
        };
        let mut drag = WaveformRegionDrag::begin_handle(
            point(px(40.0), px(10.0)),
            0.4,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::ResizeEnd,
            11,
        )
        .expect("valid resize handle");
        drag.update(point(px(61.0), px(10.0)));

        let resized = drag
            .preview_region_with_bpm(bpm_snap)
            .expect("snapped region remains valid");

        assert!(resized.end > baseline.end);
        let snapped_duration = (resized.end - resized.start) * 10.0;
        let bpm = 4.0 * 60.0 / snapped_duration;
        assert!((bpm - bpm.round()).abs() < 0.001);
    }

    #[test]
    fn bpm_snap_preserves_fixed_edge_direction_and_leaves_moves_unchanged() {
        let bounds = test_bounds(100.0, 100.0);
        let viewport = WaveformViewport::full();
        let baseline = WaveformPlayRegion::new(0.6, 0.8).unwrap();
        let bpm_snap = BpmSnapSettings {
            enabled: true,
            beat_count: 8,
            audio_duration_seconds: 12.0,
            constrain_to_audio: true,
        };
        let mut resize_start = WaveformRegionDrag::begin_handle(
            point(px(60.0), px(10.0)),
            0.6,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::ResizeStart,
            12,
        )
        .expect("valid resize handle");
        resize_start.update(point(px(30.0), px(10.0)));
        let resized = resize_start
            .preview_region_with_bpm(bpm_snap)
            .expect("snapped region remains valid");
        assert!(resized.start < baseline.start);
        assert_eq!(resized.end, baseline.end);

        let mut move_drag = WaveformRegionDrag::begin_handle(
            point(px(70.0), px(10.0)),
            0.7,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::Move,
            12,
        )
        .expect("valid move handle");
        move_drag.update(point(px(80.0), px(10.0)));
        let moved_with_snap = move_drag
            .preview_region_with_bpm(bpm_snap)
            .expect("move remains valid");
        let moved_without_snap = move_drag.preview_region().expect("move remains valid");
        assert_eq!(moved_with_snap, moved_without_snap);
    }

    #[test]
    fn bpm_snap_defaults_to_disabled_without_a_loaded_waveform() {
        let settings = BpmSnapSettings::disabled();
        assert!(!settings.enabled);
        assert_eq!(settings.beat_count, 0);
        assert_eq!(settings.audio_duration_seconds, 0.0);
    }

    #[test]
    fn waveform_region_keyboard_routing_requires_an_unmodified_waveform_key() {
        assert_eq!(
            waveform_region_navigation_key("left", false, false, false, false),
            Some(WaveformRegionSlide::Left)
        );
        assert_eq!(
            waveform_region_navigation_key("arrowright", false, false, false, false),
            Some(WaveformRegionSlide::Right)
        );
        assert_eq!(
            waveform_region_navigation_key("left", true, false, false, false),
            None
        );
        assert_eq!(
            waveform_region_navigation_key("right", false, true, false, false),
            None
        );
        assert_eq!(
            waveform_region_navigation_key("right", false, false, true, false),
            None
        );
        assert_eq!(
            waveform_region_navigation_key("right", false, false, false, true),
            None
        );
        assert_eq!(
            waveform_region_navigation_key("up", false, false, false, false),
            None
        );
    }

    #[test]
    fn stale_or_unloaded_region_drags_cannot_commit() {
        let drag = WaveformRegionDrag::begin(
            point(px(20.0), px(5.0)),
            0.2,
            test_bounds(100.0, 20.0),
            WaveformViewport::full(),
            9,
        );

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
        let viewport = WaveformViewport::full();
        let cold_start = Instant::now();
        for generation in 0..100 {
            let _ = cache.paths_for(generation, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0);
        }
        let cold = cold_start.elapsed();
        let warm_start = Instant::now();
        for _ in 0..100 {
            let _ = cache.paths_for(99, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0);
        }
        let warm = warm_start.elapsed();
        eprintln!("waveform geometry: 100 misses {cold:?}; 100 hits {warm:?}");
    }
}
