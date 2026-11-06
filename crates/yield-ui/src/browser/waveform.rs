#![deny(missing_docs)]
#![deny(warnings)]

//! Waveform shaping and canvas rendering for the browser preview panel.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use crate::beat_count_input::MAX_BEAT_COUNT;
use gpui::{
    AnyView, App, Bounds, ContentMask, Context, DragMoveEvent, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Path, PathBuilder, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, Window, canvas, div, fill, point, prelude::*, px,
};
use yield_core::{
    PlaymarkBeatGrid, WaveformEditFade, WaveformEditFadeDrag, WaveformEditFadeHandle,
    WaveformEditRegion, WaveformEditSettings, WaveformRegion, WaveformViewport,
    snap_resize_ratio_to_whole_bpm,
};
use yield_design::Theme;
use yield_session::SourcePlaymarkTempo;

use crate::theme::color;

use super::Browser;
use super::played_ranges;
use super::playmark_tempo;
use super::playmark_timing;
use super::similar_sections;
use super::transient_markers;
use super::waveform_fades;
use super::waveform_scrollbar;
use super::zero_crossing;

const WAVEFORM_SELECTION_CLICK_SLOP_PX: f32 = 2.0;
const WAVEFORM_SELECTION_EDGE_WIDTH: f32 = 1.0;
const PLAY_REGION_RESIZE_HANDLE_HEIGHT: f32 = 22.0;
const PLAY_REGION_RESIZE_HANDLE_WIDTH: f32 = 9.0;
const PLAY_REGION_RESIZE_HIT_WIDTH: f32 = 15.0;
const PLAY_REGION_MOVE_GRIP_HEIGHT: f32 = 7.0;
const PLAY_REGION_MOVE_GRIP_INSET: f32 = 9.0;
const EDIT_REGION_RESIZE_HANDLE_HEIGHT: f32 = 22.0;
const EDIT_REGION_RESIZE_HANDLE_WIDTH: f32 = 7.0;
const EDIT_REGION_RESIZE_HIT_WIDTH: f32 = 13.0;
const EDIT_REGION_MOVE_GRIP_HEIGHT: f32 = 7.0;
const EDIT_REGION_MOVE_GRIP_INSET: f32 = 9.0;
const EDIT_GAIN_HANDLE_WIDTH: f32 = 12.0;
const EDIT_GAIN_HANDLE_HEIGHT: f32 = 10.0;
const EDIT_GAIN_HANDLE_HIT_SIZE: f32 = 18.0;
const WAVEFORM_PAN_REFERENCE_WIDTH: f64 = 1_200.0;
const WAVEFORM_WHEEL_EPSILON: f64 = f64::EPSILON;
const BPM_SNAP_ICON_SIZE: f32 = 16.0;
const BEAT_GUIDE_WIDTH: f32 = 1.0;
const BEAT_GUIDE_HEIGHT_FRACTION: f32 = 0.72;

/// Return the default settings for a newly created edit selection.
pub(super) fn default_edit_settings() -> WaveformEditSettings {
    WaveformEditSettings::new(1.0, None, None).expect("the unity edit settings baseline is finite")
}

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

/// Render the fixed 16×16 beat-guide glyph used by the compact toolbar control.
pub(super) fn render_beat_guides_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            for (left, top, right, bottom) in [
                (2.0, 3.0, 3.5, 13.0),
                (5.5, 5.0, 6.5, 11.0),
                (9.5, 5.0, 10.5, 11.0),
                (12.5, 3.0, 14.0, 13.0),
                (2.0, 7.25, 14.0, 8.75),
            ] {
                let guide = Bounds::from_corners(
                    bpm_snap_icon_point(bounds, left, top),
                    bpm_snap_icon_point(bounds, right, bottom),
                );
                window.paint_quad(fill(guide, icon_color));
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
                "Paint new playmarks in whole beats at the authored tempo. Snap resized selections to whole BPM values.",
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
            .child("Beat guide divisions. Drag up or down to adjust.")
    }
}

pub(super) fn beat_count_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| BeatCountTooltip).into()
}

struct BeatGuidesTooltip;

impl Render for BeatGuidesTooltip {
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
            .child("Show beat guide lines inside the play selection.")
    }
}

/// Build the beat-guide tooltip view for the compact toolbar control.
pub(super) fn beat_guides_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| BeatGuidesTooltip).into()
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

    pub(super) fn core(self) -> Option<WaveformRegion> {
        WaveformRegion::new(self.start, self.end)
    }

    pub(super) fn frame_bounds(self, total_frames: usize) -> Option<(usize, usize)> {
        if total_frames == 0 || !self.is_valid() {
            return None;
        }
        // Playback rounds normalized span endpoints to frames. Use those same frames instead
        // of using the outward-rounded range used for timing labels.
        let total = total_frames as f64;
        let start = ((self.start * total).round() as usize).min(total_frames - 1);
        let end = ((self.end * total).round() as usize).clamp(start + 1, total_frames);
        Some((start, end))
    }

    fn shifted_by_frames(
        self,
        direction: WaveformRegionSlide,
        total_frames: usize,
    ) -> Option<Self> {
        let (start, end) = self.frame_bounds(total_frames)?;
        let width = end - start;
        let next_start = match direction {
            WaveformRegionSlide::Left => start.checked_sub(width)?,
            WaveformRegionSlide::Right => {
                let next = start + width;
                (next <= total_frames - width).then_some(next)?
            }
            WaveformRegionSlide::FineLeft => start.saturating_sub(1),
            WaveformRegionSlide::FineRight => start.saturating_add(1).min(total_frames - width),
        };
        let total = total_frames as f64;
        Self::new(
            next_start as f64 / total,
            (next_start + width) as f64 / total,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveformRegionHandle {
    ResizeStart,
    ResizeEnd,
    Move,
}

/// The committed waveform selection that owns a pointer gesture or snap result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveformSelectionKind {
    /// The playback selection, which remains clamped to the audio interval.
    Play,
    /// The edit selection, which may retain finite silence-margin bounds.
    Edit,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WaveformRegionDragMode {
    Create {
        kind: WaveformSelectionKind,
    },
    MovePlay {
        baseline: WaveformRegion,
    },
    ResizePlayStart {
        baseline: WaveformRegion,
    },
    ResizePlayEnd {
        baseline: WaveformRegion,
    },
    MoveEdit {
        baseline: WaveformEditRegion,
    },
    ResizeEditStart {
        baseline: WaveformEditRegion,
    },
    ResizeEditEnd {
        baseline: WaveformEditRegion,
    },
    EditGain {
        baseline: WaveformEditRegion,
        anchor_y: f32,
    },
    EditFade {
        baseline: WaveformEditRegion,
        drag: WaveformEditFadeDrag,
    },
    EditFadeOuterGain {
        baseline: WaveformEditRegion,
        leading: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PlaymarkTempoResize {
    total_frames: usize,
    sample_rate: u32,
    beat_count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WaveformRegionDrag {
    anchor: f64,
    current: f64,
    anchor_x: f32,
    anchor_y: f32,
    current_y: f32,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    moved: bool,
    generation: u64,
    button: MouseButton,
    mode: WaveformRegionDragMode,
    edit_settings: Option<WaveformEditSettings>,
    tempo_resize: Option<PlaymarkTempoResize>,
    paint_grid: Option<PlaymarkBeatGrid>,
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
            MouseButton::Left,
            None,
            WaveformRegionDragMode::Create {
                kind: WaveformSelectionKind::Play,
            },
        )
    }

    fn begin_edit(
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
            MouseButton::Right,
            Some(default_edit_settings()),
            WaveformRegionDragMode::Create {
                kind: WaveformSelectionKind::Edit,
            },
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
            WaveformRegionHandle::ResizeStart => {
                WaveformRegionDragMode::ResizePlayStart { baseline }
            }
            WaveformRegionHandle::ResizeEnd => WaveformRegionDragMode::ResizePlayEnd { baseline },
            WaveformRegionHandle::Move => WaveformRegionDragMode::MovePlay { baseline },
        };
        Some(Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            MouseButton::Left,
            None,
            mode,
        ))
    }

    #[cfg(test)]
    fn begin_edit_handle(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformEditRegion,
        handle: WaveformRegionHandle,
        generation: u64,
    ) -> Self {
        Self::begin_edit_handle_with_button(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            handle,
            generation,
            MouseButton::Right,
            default_edit_settings(),
        )
    }

    fn begin_edit_handle_with_button(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformEditRegion,
        handle: WaveformRegionHandle,
        generation: u64,
        button: MouseButton,
        edit_settings: WaveformEditSettings,
    ) -> Self {
        let mode = match handle {
            WaveformRegionHandle::ResizeStart => {
                WaveformRegionDragMode::ResizeEditStart { baseline: region }
            }
            WaveformRegionHandle::ResizeEnd => {
                WaveformRegionDragMode::ResizeEditEnd { baseline: region }
            }
            WaveformRegionHandle::Move => WaveformRegionDragMode::MoveEdit { baseline: region },
        };
        Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            button,
            Some(edit_settings),
            mode,
        )
    }

    fn begin_edit_gain(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        generation: u64,
        button: MouseButton,
    ) -> Self {
        Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            button,
            Some(settings),
            WaveformRegionDragMode::EditGain {
                baseline: region,
                anchor_y: f32::from(position.y),
            },
        )
    }

    fn begin_edit_fade(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        generation: u64,
        button: MouseButton,
        handle: WaveformEditFadeHandle,
    ) -> Self {
        Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            button,
            Some(settings),
            WaveformRegionDragMode::EditFade {
                baseline: region,
                drag: WaveformEditFadeDrag::new(handle, region, settings),
            },
        )
    }

    fn begin_edit_fade_outer_gain(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        generation: u64,
        button: MouseButton,
        leading: bool,
    ) -> Self {
        Self::with_mode(
            position,
            visible_ratio,
            bounds,
            viewport,
            generation,
            button,
            Some(settings),
            WaveformRegionDragMode::EditFadeOuterGain {
                baseline: region,
                leading,
            },
        )
    }

    fn with_mode(
        position: Point<Pixels>,
        visible_ratio: f64,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        generation: u64,
        button: MouseButton,
        edit_settings: Option<WaveformEditSettings>,
        mode: WaveformRegionDragMode,
    ) -> Self {
        let ratio = viewport.absolute_ratio_from_visible(visible_ratio);
        Self {
            anchor: ratio,
            current: ratio,
            anchor_x: f32::from(position.x),
            anchor_y: f32::from(position.y),
            current_y: f32::from(position.y),
            bounds,
            viewport,
            moved: false,
            generation,
            button,
            mode,
            edit_settings,
            tempo_resize: None,
            paint_grid: None,
        }
    }

    fn capture_tempo_resize(&mut self, total_frames: usize, sample_rate: u32, beat_count: u8) {
        if total_frames > 0
            && sample_rate > 0
            && beat_count > 0
            && matches!(
                self.mode,
                WaveformRegionDragMode::ResizePlayStart { .. }
                    | WaveformRegionDragMode::ResizePlayEnd { .. }
            )
        {
            self.tempo_resize = Some(PlaymarkTempoResize {
                total_frames,
                sample_rate,
                beat_count,
            });
        }
    }

    fn capture_paint_grid(&mut self, total_frames: usize, sample_rate: u32, bpm: f64) {
        if matches!(
            self.mode,
            WaveformRegionDragMode::Create {
                kind: WaveformSelectionKind::Play
            }
        ) {
            let anchor = (self.anchor.clamp(0.0, 1.0) * total_frames as f64).round() as usize;
            self.paint_grid = PlaymarkBeatGrid::new(anchor, total_frames, sample_rate, bpm);
        }
    }

    fn snapped_paint_region(self, grid: PlaymarkBeatGrid) -> Option<WaveformPlayRegion> {
        let total = grid.total_frames() as f64;
        let pointer = (self.current.clamp(0.0, 1.0) * total).round() as usize;
        let snapped = grid.snap(pointer, self.current < self.anchor)?;
        let (first, end) = snapped.frames;
        WaveformPlayRegion::new(first as f64 / total, end as f64 / total)
    }

    fn tempo_resize_region(self, capture: PlaymarkTempoResize) -> Option<WaveformPlayRegion> {
        let (baseline, leading) = match self.mode {
            WaveformRegionDragMode::ResizePlayStart { baseline } => (baseline, true),
            WaveformRegionDragMode::ResizePlayEnd { baseline } => (baseline, false),
            _ => return None,
        };
        let baseline = WaveformPlayRegion::from_core(baseline);
        let (start, end) = baseline.frame_bounds(capture.total_frames)?;
        let total = capture.total_frames as f64;
        let moving = (self.current.clamp(0.0, 1.0) * total).round() as usize;
        let (start, end) = if leading {
            (moving.min(end - 1), end)
        } else {
            (start, moving.max(start + 1).min(capture.total_frames))
        };
        WaveformPlayRegion::new(start as f64 / total, end as f64 / total)
    }

    fn update(&mut self, position: Point<Pixels>) {
        if let Some(ratio) = waveform_click_ratio(position, self.bounds) {
            self.current = self.viewport.absolute_ratio_from_visible(ratio);
        }
        self.current_y = f32::from(position.y);
        let moved = match self.mode {
            WaveformRegionDragMode::EditGain { anchor_y, .. } => {
                (self.current_y - anchor_y).abs() > WAVEFORM_SELECTION_CLICK_SLOP_PX
            }
            WaveformRegionDragMode::EditFadeOuterGain { .. } => {
                (self.current_y - self.anchor_y).abs() > WAVEFORM_SELECTION_CLICK_SLOP_PX
            }
            _ => (f32::from(position.x) - self.anchor_x).abs() > WAVEFORM_SELECTION_CLICK_SLOP_PX,
        };
        self.moved |= moved;
    }

    fn preview_region(self) -> Option<WaveformPlayRegion> {
        self.preview_region_with_bpm(BpmSnapSettings::disabled())
    }

    pub(super) fn preview_region_with_bpm(
        self,
        bpm_snap: BpmSnapSettings,
    ) -> Option<WaveformPlayRegion> {
        if self.moved
            && let Some(capture) = self.tempo_resize
        {
            return self.tempo_resize_region(capture);
        }
        if !self.moved {
            match self.mode {
                WaveformRegionDragMode::Create {
                    kind: WaveformSelectionKind::Play,
                } => {}
                WaveformRegionDragMode::MovePlay { baseline }
                | WaveformRegionDragMode::ResizePlayStart { baseline }
                | WaveformRegionDragMode::ResizePlayEnd { baseline } => {
                    return Some(WaveformPlayRegion::from_core(baseline));
                }
                _ => return None,
            }
        }
        match self.mode {
            WaveformRegionDragMode::Create {
                kind: WaveformSelectionKind::Play,
            } => match self.paint_grid {
                Some(grid) => self
                    .moved
                    .then(|| self.snapped_paint_region(grid))
                    .flatten(),
                None => WaveformPlayRegion::new(self.anchor, self.current),
            },
            WaveformRegionDragMode::MovePlay { baseline } => baseline
                .moved_by(self.current - self.anchor)
                .map(WaveformPlayRegion::from_core),
            WaveformRegionDragMode::ResizePlayStart { baseline } => baseline
                .resized_start_to(snapped_resize_edge(baseline.end(), self.current, bpm_snap))
                .map(WaveformPlayRegion::from_core),
            WaveformRegionDragMode::ResizePlayEnd { baseline } => baseline
                .resized_end_to(snapped_resize_edge(
                    baseline.start(),
                    self.current,
                    bpm_snap,
                ))
                .map(WaveformPlayRegion::from_core),
            _ => None,
        }
    }

    fn preview_edit_region(self) -> Option<WaveformEditRegion> {
        self.preview_edit_region_with_bpm(BpmSnapSettings::disabled())
    }

    pub(super) fn preview_edit_region_with_bpm(
        self,
        bpm_snap: BpmSnapSettings,
    ) -> Option<WaveformEditRegion> {
        if !self.moved {
            match self.mode {
                WaveformRegionDragMode::Create {
                    kind: WaveformSelectionKind::Edit,
                } => {}
                WaveformRegionDragMode::MoveEdit { baseline }
                | WaveformRegionDragMode::ResizeEditStart { baseline }
                | WaveformRegionDragMode::ResizeEditEnd { baseline }
                | WaveformRegionDragMode::EditGain { baseline, .. }
                | WaveformRegionDragMode::EditFade { baseline, .. }
                | WaveformRegionDragMode::EditFadeOuterGain { baseline, .. } => {
                    return Some(baseline);
                }
                _ => return None,
            }
        }
        match self.mode {
            WaveformRegionDragMode::Create {
                kind: WaveformSelectionKind::Edit,
            } => WaveformEditRegion::new(self.anchor, self.current),
            WaveformRegionDragMode::MoveEdit { baseline } => {
                baseline.moved_by(self.current - self.anchor)
            }
            WaveformRegionDragMode::ResizeEditStart { baseline } => baseline
                .resized_start_to(snapped_resize_edge(baseline.end(), self.current, bpm_snap)),
            WaveformRegionDragMode::ResizeEditEnd { baseline } => baseline.resized_end_to(
                snapped_resize_edge(baseline.start(), self.current, bpm_snap),
            ),
            WaveformRegionDragMode::EditGain { baseline, .. } => Some(baseline),
            WaveformRegionDragMode::EditFade { baseline, drag } => drag
                .apply(
                    baseline,
                    self.edit_settings.unwrap_or_else(default_edit_settings),
                    self.current,
                )
                .map(|(region, _)| region),
            WaveformRegionDragMode::EditFadeOuterGain { baseline, .. } => Some(baseline),
            _ => None,
        }
    }

    pub(super) fn preview_edit_settings(self) -> WaveformEditSettings {
        let Some(region) = self.preview_edit_region() else {
            return self.edit_settings.unwrap_or_else(default_edit_settings);
        };
        self.preview_edit_settings_for_region(region)
    }

    pub(super) fn preview_edit_settings_for_region(
        self,
        region: WaveformEditRegion,
    ) -> WaveformEditSettings {
        let settings = self.edit_settings.unwrap_or_else(default_edit_settings);
        match self.mode {
            WaveformRegionDragMode::EditGain { anchor_y, .. } => {
                settings.with_gain_for_pointer_y(anchor_y, self.current_y)
            }
            WaveformRegionDragMode::EditFade { baseline, drag } => drag
                .apply(baseline, settings, self.current)
                .map(|(_, settings)| settings)
                .unwrap_or(settings),
            WaveformRegionDragMode::EditFadeOuterGain { leading, .. } => {
                self.preview_outer_gain_settings(settings, leading)
            }
            WaveformRegionDragMode::ResizeEditStart { baseline }
            | WaveformRegionDragMode::ResizeEditEnd { baseline } => {
                settings.for_resized_region(baseline, region)
            }
            _ => settings,
        }
    }

    fn preview_outer_gain_settings(
        self,
        settings: WaveformEditSettings,
        leading: bool,
    ) -> WaveformEditSettings {
        let height = f32::from(self.bounds.size.height);
        let top = f32::from(self.bounds.origin.y);
        let vertical_ratio = if height.is_finite() && height > 0.0 && top.is_finite() {
            ((self.current_y - top) / height).clamp(0.0, 1.0)
        } else {
            f32::NAN
        };
        let fade_in = settings.fade_in();
        let fade_out = settings.fade_out();
        let (fade_in, fade_out) = if leading {
            (
                fade_in.map(|fade| fade.with_outer_gain_for_vertical_ratio(vertical_ratio)),
                fade_out,
            )
        } else {
            (
                fade_in,
                fade_out.map(|fade| fade.with_outer_gain_for_vertical_ratio(vertical_ratio)),
            )
        };
        WaveformEditSettings::new(settings.gain(), fade_in, fade_out).unwrap_or(settings)
    }

    pub(super) fn selection_kind(self) -> WaveformSelectionKind {
        match self.mode {
            WaveformRegionDragMode::Create { kind } => kind,
            WaveformRegionDragMode::MovePlay { .. }
            | WaveformRegionDragMode::ResizePlayStart { .. }
            | WaveformRegionDragMode::ResizePlayEnd { .. } => WaveformSelectionKind::Play,
            WaveformRegionDragMode::MoveEdit { .. }
            | WaveformRegionDragMode::ResizeEditStart { .. }
            | WaveformRegionDragMode::ResizeEditEnd { .. }
            | WaveformRegionDragMode::EditGain { .. }
            | WaveformRegionDragMode::EditFade { .. }
            | WaveformRegionDragMode::EditFadeOuterGain { .. } => WaveformSelectionKind::Edit,
        }
    }

    pub(super) fn button(self) -> MouseButton {
        self.button
    }

    pub(super) fn preview_bounds_with_bpm(self, bpm_snap: BpmSnapSettings) -> Option<(f64, f64)> {
        match self.selection_kind() {
            WaveformSelectionKind::Play => self
                .preview_region_with_bpm(bpm_snap)
                .map(|region| (region.start, region.end)),
            WaveformSelectionKind::Edit => self
                .preview_edit_region_with_bpm(bpm_snap)
                .map(|region| (region.start(), region.end())),
        }
    }

    fn ratio_at(self, position: Point<Pixels>) -> Option<f64> {
        waveform_click_ratio(position, self.bounds)
            .map(|ratio| self.viewport.absolute_ratio_from_visible(ratio))
    }

    fn is_handle_drag(self) -> bool {
        !matches!(self.mode, WaveformRegionDragMode::Create { .. })
    }

    pub(super) fn is_edit_gain(self) -> bool {
        matches!(self.mode, WaveformRegionDragMode::EditGain { .. })
    }

    pub(super) fn is_edit_fade(self) -> bool {
        matches!(
            self.mode,
            WaveformRegionDragMode::EditFade { .. }
                | WaveformRegionDragMode::EditFadeOuterGain { .. }
        )
    }

    fn is_edit_effect_handle(self) -> bool {
        self.is_edit_gain() || self.is_edit_fade()
    }

    pub(super) fn edit_resize_effects(self) -> Option<(WaveformEditRegion, WaveformEditSettings)> {
        let baseline = match self.mode {
            WaveformRegionDragMode::ResizeEditStart { baseline }
            | WaveformRegionDragMode::ResizeEditEnd { baseline } => baseline,
            _ => return None,
        };
        Some((
            baseline,
            self.edit_settings.unwrap_or_else(default_edit_settings),
        ))
    }

    pub(super) fn zero_crossing_applies(self, bpm_snap: BpmSnapSettings) -> bool {
        self.moved
            && self.tempo_resize.is_none()
            && self.paint_grid.is_none()
            && !self.is_edit_effect_handle()
            && (!self.is_resize() || !bpm_snap.enabled)
    }

    fn is_resize(self) -> bool {
        matches!(
            self.mode,
            WaveformRegionDragMode::ResizePlayStart { .. }
                | WaveformRegionDragMode::ResizePlayEnd { .. }
                | WaveformRegionDragMode::ResizeEditStart { .. }
                | WaveformRegionDragMode::ResizeEditEnd { .. }
        )
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

/// Iterate the internal beat-guide positions that are visible in a waveform viewport.
///
/// The iterator is bounded by [`MAX_BEAT_COUNT`]. Guide positions at the selection edges are
/// intentionally omitted; only the divisions inside the selected region are returned.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BeatGuideProjection {
    start: f64,
    end: f64,
    viewport: WaveformViewport,
    beat_count: u8,
    next_index: u8,
}

impl BeatGuideProjection {
    fn new(start: f64, end: f64, viewport: WaveformViewport, beat_count: u8) -> Self {
        Self {
            start,
            end,
            viewport,
            beat_count: beat_count.min(MAX_BEAT_COUNT),
            next_index: 1,
        }
    }
}

impl Iterator for BeatGuideProjection {
    type Item = f64;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index < self.beat_count {
            let index = self.next_index;
            self.next_index = self.next_index.saturating_add(1);
            let fraction = f64::from(index) / f64::from(self.beat_count);
            let absolute_ratio = self.start + (self.end - self.start) * fraction;
            let Some(visible_ratio) = self.viewport.visible_ratio_from_absolute(absolute_ratio)
            else {
                continue;
            };
            return Some(visible_ratio);
        }
        None
    }
}

fn beat_guide_projection(
    start: f64,
    end: f64,
    viewport: WaveformViewport,
    beat_count: u8,
) -> BeatGuideProjection {
    BeatGuideProjection::new(start, end, viewport, beat_count)
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

#[derive(Clone)]
struct WaveformDragToken(gpui::WeakEntity<Browser>);

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
    paths: Rc<Vec<(usize, Path<Pixels>)>>,
    gain_preview_key: Option<WaveformGainPreviewKey>,
    gain_preview_geometry: Option<WaveformGainPreviewGeometry>,
    #[cfg(test)]
    rebuild_count: usize,
    #[cfg(test)]
    gain_preview_rebuild_count: usize,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaveformGainPreviewKind {
    Bands,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WaveformGainPreviewKey {
    base: WaveformCacheKey,
    kind: WaveformGainPreviewKind,
    visible_start_bits: u64,
    visible_end_bits: u64,
    selection: WaveformEditRegion,
    settings: WaveformEditSettings,
    preview_len: usize,
}

#[derive(Clone)]
enum WaveformGainPreviewGeometry {
    Bands {
        selection_bounds: Bounds<Pixels>,
        paths: Rc<Vec<(usize, Path<Pixels>)>>,
    },
}

impl WaveformRenderCache {
    fn base_key(
        generation: u64,
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
    ) -> WaveformCacheKey {
        WaveformCacheKey {
            generation,
            bounds,
            peak_scale_bits: peak_scale.to_bits(),
            viewport_start_bits: viewport.start.to_bits(),
            viewport_end_bits: viewport.end.to_bits(),
            projection_revision,
            projection_start_bits: source_range.0.to_bits(),
            projection_end_bits: source_range.1.to_bits(),
        }
    }

    fn ensure_base_paths(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
    ) -> WaveformCacheKey {
        let key = Self::base_key(
            generation,
            peak_scale,
            bounds,
            viewport,
            source_range,
            projection_revision,
        );
        let mut state = self.state.borrow_mut();
        if state.key != Some(key) {
            state.paths = Rc::new(build_waveform_paths(
                bands,
                peak_scale,
                bounds,
                viewport,
                source_range,
            ));
            state.key = Some(key);
            state.gain_preview_key = None;
            state.gain_preview_geometry = None;
            #[cfg(test)]
            {
                state.rebuild_count += 1;
            }
        }
        key
    }

    /// Share immutable paths for this exact preview generation and layout.
    /// Cache retrieval never copies vertices; GPUI's owned paint API copies each path at drawing.
    fn paths_for(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
    ) -> Rc<Vec<(usize, Path<Pixels>)>> {
        self.ensure_base_paths(
            generation,
            bands,
            peak_scale,
            bounds,
            viewport,
            source_range,
            projection_revision,
        );
        self.state.borrow().paths.clone()
    }

    #[cfg(test)]
    fn gain_preview_paths_for(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
        selection: WaveformEditRegion,
        gain: f32,
    ) -> Option<WaveformGainPreviewGeometry> {
        self.edit_preview_paths_for(
            generation,
            bands,
            peak_scale,
            bounds,
            viewport,
            source_range,
            projection_revision,
            selection,
            WaveformEditSettings::new(gain, None, None).unwrap_or_else(default_edit_settings),
        )
    }

    fn edit_preview_paths_for(
        &self,
        generation: u64,
        bands: &[[f32; 4]],
        peak_scale: f32,
        bounds: Bounds<Pixels>,
        viewport: WaveformViewport,
        source_range: (f64, f64),
        projection_revision: u64,
        selection: WaveformEditRegion,
        settings: WaveformEditSettings,
    ) -> Option<WaveformGainPreviewGeometry> {
        let base = self.ensure_base_paths(
            generation,
            bands,
            peak_scale,
            bounds,
            viewport,
            source_range,
            projection_revision,
        );
        let affected = edit_effect_bounds(selection, settings);
        let (visible_start, visible_end) =
            viewport.visible_range_from_absolute(affected.start(), affected.end())?;
        let key = WaveformGainPreviewKey {
            base,
            kind: WaveformGainPreviewKind::Bands,
            visible_start_bits: visible_start.to_bits(),
            visible_end_bits: visible_end.to_bits(),
            selection,
            settings,
            preview_len: bands.len(),
        };
        let mut state = self.state.borrow_mut();
        if state.gain_preview_key != Some(key) {
            let selection_bounds = visible_selection_bounds(bounds, visible_start, visible_end)?;
            let point_count = drawable_point_count(bounds.size.width, bands.len());
            let extents = resample_edit_preview_extents(
                bands,
                point_count,
                peak_scale,
                viewport,
                source_range,
                selection,
                visible_start,
                visible_end,
                settings,
            );
            let paths = Rc::new(build_waveform_paths_from_extents(
                &extents,
                selection_bounds,
            ));
            state.gain_preview_key = Some(key);
            state.gain_preview_geometry = Some(WaveformGainPreviewGeometry::Bands {
                selection_bounds,
                paths,
            });
            #[cfg(test)]
            {
                state.gain_preview_rebuild_count += 1;
            }
        }
        match state.gain_preview_geometry.as_ref()? {
            WaveformGainPreviewGeometry::Bands {
                selection_bounds,
                paths,
            } => Some(WaveformGainPreviewGeometry::Bands {
                selection_bounds: *selection_bounds,
                paths: paths.clone(),
            }),
        }
    }

    #[cfg(test)]
    fn rebuild_count(&self) -> usize {
        self.state.borrow().rebuild_count
    }

    #[cfg(test)]
    fn gain_preview_rebuild_count(&self) -> usize {
        self.state.borrow().gain_preview_rebuild_count
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
    build_waveform_paths_from_extents(&extents, bounds)
}

fn build_waveform_paths_from_extents(
    extents: &[[f32; 4]],
    bounds: Bounds<Pixels>,
) -> Vec<(usize, Path<Pixels>)> {
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

fn visible_selection_bounds(
    bounds: Bounds<Pixels>,
    visible_start: f64,
    visible_end: f64,
) -> Option<Bounds<Pixels>> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if !width.is_finite()
        || width <= 0.0
        || !height.is_finite()
        || height <= 0.0
        || !visible_start.is_finite()
        || !visible_end.is_finite()
        || visible_end <= visible_start
    {
        return None;
    }
    let left = f32::from(bounds.origin.x) + width * visible_start as f32;
    let right = f32::from(bounds.origin.x) + width * visible_end as f32;
    Some(Bounds::from_corners(
        point(px(left), bounds.origin.y),
        point(px(right), bounds.bottom()),
    ))
}

/// Rebuild only the selected visible span after applying gain to raw signal lanes.
///
/// The static waveform path shapes each source column before max pooling. Gain must be applied
/// to the raw lanes first, matching the native signal preview's clamp-before-visualization order.
#[cfg(test)]
fn resample_gain_preview_extents(
    bands: &[[f32; 4]],
    point_count: usize,
    peak_scale: f32,
    viewport: WaveformViewport,
    source_range: (f64, f64),
    selection: WaveformEditRegion,
    visible_start: f64,
    visible_end: f64,
    gain: f32,
) -> Vec<[f32; 4]> {
    resample_edit_preview_extents(
        bands,
        point_count,
        peak_scale,
        viewport,
        source_range,
        selection,
        visible_start,
        visible_end,
        WaveformEditSettings::new(gain, None, None).unwrap_or_else(default_edit_settings),
    )
}

fn resample_edit_preview_extents(
    bands: &[[f32; 4]],
    point_count: usize,
    peak_scale: f32,
    viewport: WaveformViewport,
    source_range: (f64, f64),
    selection: WaveformEditRegion,
    visible_start: f64,
    visible_end: f64,
    settings: WaveformEditSettings,
) -> Vec<[f32; 4]> {
    if bands.is_empty() || point_count < 2 {
        return Vec::new();
    }
    let start = viewport.absolute_ratio_from_visible(visible_start);
    let end = viewport.absolute_ratio_from_visible(visible_end);
    let width = end - start;
    let source_width = source_range.1 - source_range.0;
    if !width.is_finite() || width <= 0.0 || !source_width.is_finite() || source_width <= 0.0 {
        return Vec::new();
    }
    let count = ((point_count as f64 * (visible_end - visible_start)).ceil() as usize)
        .clamp(2, point_count);
    let mut extents = vec![[0.0_f32; 4]; count];
    let overlap_start = start.max(source_range.0);
    let overlap_end = end.min(source_range.1);
    if overlap_end <= overlap_start {
        return extents;
    }
    let band_count = bands.len() as f64;
    let first_source = (((overlap_start - source_range.0) / source_width) * band_count)
        .floor()
        .clamp(0.0, band_count - 1.0) as usize;
    let last_source = (((overlap_end - source_range.0) / source_width) * band_count)
        .ceil()
        .clamp((first_source + 1) as f64, band_count) as usize;
    let gains = (0..count)
        .map(|index| {
            settings
                .visual_gain_at_ratio(selection, start + width * index as f64 / (count - 1) as f64)
        })
        .collect::<Vec<_>>();
    for (offset, column) in bands[first_source..last_source].iter().copied().enumerate() {
        let index = first_source + offset;
        let band_start = source_range.0 + index as f64 / band_count * source_width;
        let band_end = source_range.0 + (index + 1) as f64 / band_count * source_width;
        let first = (((band_start.max(start) - start) / width) * count as f64)
            .floor()
            .clamp(0.0, (count - 1) as f64) as usize;
        let last = (((band_end.min(end) - start) / width) * count as f64)
            .ceil()
            .clamp((first + 1) as f64, count as f64) as usize;
        for bucket in first..last {
            let scaled = column.map(|value| (value * gains[bucket]).clamp(0.0, 1.0));
            for lane in 0..4 {
                extents[bucket][lane] =
                    extents[bucket][lane].max(waveform_extent(scaled, lane, peak_scale));
            }
        }
    }
    extents
}

fn edit_effect_bounds(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
) -> WaveformEditRegion {
    let start = region.start()
        - region.width()
            * f64::from(
                settings
                    .fade_in()
                    .map_or(0.0, yield_core::WaveformEditFade::mute),
            );
    let end = region.end()
        + region.width()
            * f64::from(
                settings
                    .fade_out()
                    .map_or(0.0, yield_core::WaveformEditFade::mute),
            );
    WaveformEditRegion::new(
        if start.is_finite() {
            start
        } else {
            region.start()
        },
        if end.is_finite() { end } else { region.end() },
    )
    .unwrap_or(region)
}

fn visible_peak_values(peaks: &[f32], viewport: WaveformViewport) -> Vec<f32> {
    if peaks.is_empty() {
        return Vec::new();
    }
    let point_count =
        ((peaks.len() as f64 * viewport.width()).ceil() as usize).clamp(1, peaks.len());
    let mut visible = vec![0.0_f32; point_count];
    let peak_count = peaks.len() as f64;
    let first_source = (viewport.start * peak_count)
        .floor()
        .clamp(0.0, (peaks.len().saturating_sub(1)) as f64) as usize;
    let last_source = (viewport.end * peak_count)
        .ceil()
        .clamp((first_source + 1) as f64, peaks.len() as f64) as usize;
    for (index, peak) in peaks[first_source..last_source].iter().copied().enumerate() {
        let index = first_source + index;
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

fn gain_for_visible_peak(
    index: usize,
    visible_peaks: &[f32],
    viewport: WaveformViewport,
    edit: Option<(WaveformEditRegion, WaveformEditSettings)>,
) -> Option<f32> {
    let center = viewport
        .absolute_ratio_from_visible((index as f64 + 0.5) / visible_peaks.len().max(1) as f64);
    edit.filter(|(region, settings)| {
        let affected = edit_effect_bounds(*region, *settings);
        center >= affected.start() && center <= affected.end()
    })
    .map(|(region, settings)| settings.visual_gain_at_ratio(region, center))
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
    let drag = browser.waveform_region_drag;
    let play_region = drag
        .and_then(|drag| zero_crossing::preview_drag_region(browser, drag))
        .or(browser.waveform_play_region);
    let edit_region = drag
        .and_then(|drag| zero_crossing::preview_drag_edit_region(browser, drag))
        .or(browser.waveform_edit_region);
    let edit_drag = drag.filter(|drag| drag.selection_kind() == WaveformSelectionKind::Edit);
    let edit_settings = edit_drag
        .and_then(|drag| edit_region.map(|region| drag.preview_edit_settings_for_region(region)))
        .unwrap_or(browser.waveform_edit_settings);
    let gain_preview = edit_region
        .filter(|_| {
            (edit_settings.gain() - 1.0).abs() > f32::EPSILON
                || edit_settings.fade_in().is_some()
                || edit_settings.fade_out().is_some()
        })
        .map(|region| (region, edit_settings));
    let mut bars = div()
        .id("waveform")
        .when_some(
            browser.ax_marker("transport-waveform"),
            |element, marker| element.child(marker),
        )
        .debug_selector(|| String::from("waveform"))
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
                    let paths = cache.paths_for(
                        generation,
                        &bands,
                        peak_scale,
                        bounds,
                        viewport,
                        source_range,
                        projection_revision,
                    );
                    if let Some((selection, settings)) = gain_preview
                        && let Some(WaveformGainPreviewGeometry::Bands {
                            selection_bounds,
                            paths: preview_paths,
                        }) = cache.edit_preview_paths_for(
                            generation,
                            &bands,
                            peak_scale,
                            bounds,
                            viewport,
                            source_range,
                            projection_revision,
                            selection,
                            settings,
                        )
                    {
                        let bounds_origin = bounds.origin;
                        let bounds_bottom = bounds.bottom();
                        let left_bounds = Bounds::from_corners(
                            bounds_origin,
                            point(selection_bounds.left(), bounds_bottom),
                        );
                        let right_bounds = Bounds::from_corners(
                            point(selection_bounds.right(), bounds_origin.y),
                            point(bounds.right(), bounds_bottom),
                        );
                        for clip_bounds in [left_bounds, right_bounds] {
                            if clip_bounds.size.width <= px(0.0) {
                                continue;
                            }
                            window.with_content_mask(
                                Some(ContentMask {
                                    bounds: clip_bounds,
                                }),
                                |window| {
                                    for (lane, path) in paths.iter() {
                                        window.paint_path(path.clone(), lane_colors[*lane]);
                                    }
                                },
                            );
                        }
                        window.with_content_mask(
                            Some(ContentMask {
                                bounds: selection_bounds,
                            }),
                            |window| {
                                for (lane, path) in preview_paths.iter() {
                                    window.paint_path(path.clone(), lane_colors[*lane]);
                                }
                            },
                        );
                    } else {
                        for (lane, path) in paths.iter() {
                            window.paint_path(path.clone(), lane_colors[*lane]);
                        }
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
            let selected_path = browser
                .selected
                .and_then(|index| browser.entries.get(index))
                .map(|entry| entry.path.as_path());
            let bar_color = if browser.playing_path.as_deref() == selected_path
                && browser.playing_path.is_some()
            {
                theme.colors.waveform_played
            } else {
                theme.colors.waveform_unplayed
            };
            let visible_peaks = visible_peak_values(&preview.peaks, viewport);
            for (index, peak) in visible_peaks.iter().copied().enumerate() {
                let gain = gain_for_visible_peak(index, &visible_peaks, viewport, gain_preview);
                let peak = gain
                    .map(|gain| (peak * gain).clamp(0.0, 1.0))
                    .unwrap_or(peak);
                let height = if gain.is_some() && peak <= 0.0 {
                    0.0
                } else {
                    (peak * theme.waveform.peak_scale * available_height)
                        .max(theme.spacing.waveform_min_bar_height)
                };
                bars = bars.child(div().flex_1().h(px(height)).bg(color(bar_color)));
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
    bars = bars.child(similar_sections::overlay(browser, theme.clone()));
    bars = bars.child(transient_markers::render_overlay(browser, &theme));
    let tempo_resize = browser
        .waveform_region_drag
        .and_then(|drag| drag.tempo_resize);
    let snapped_paint = drag.is_some_and(|drag| drag.moved && drag.paint_grid.is_some());
    let beat_count = tempo_resize.map_or(browser.beat_guide_count, |capture| capture.beat_count);
    let beat_guides = (!snapped_paint && (browser.beat_guides_enabled || tempo_resize.is_some()))
        .then_some((
            beat_count,
            color(theme.colors.waveform_playhead).opacity(150.0 / 255.0),
        ));
    let active_selection_kind = browser
        .waveform_region_drag
        .map(|drag| drag.selection_kind());
    if let Some(region) = play_region {
        bars = bars.child(waveform_region_overlay(
            region,
            browser.waveform_viewport,
            color(theme.colors.selection_fill),
            color(theme.colors.waveform_playhead),
            beat_guides_for_region(
                beat_guides,
                active_selection_kind,
                WaveformSelectionKind::Play,
            ),
        ));
        if let Some(preview) = browser.waveform.as_ref() {
            let tempo_label = tempo_resize.and_then(|capture| {
                let (start, end) = region.frame_bounds(capture.total_frames)?;
                yield_core::playmark_frame_tempo_label(
                    end - start,
                    capture.sample_rate,
                    capture.beat_count,
                )
            });
            bars = bars.child(playmark_timing::render_overlay(
                region,
                browser.waveform_viewport,
                preview.total_frames,
                preview.sample_rate,
                (browser.beat_guides_enabled && !snapped_paint).then_some(browser.beat_guide_count),
                tempo_label,
                color(theme.colors.waveform_playmark_label_background),
                color(theme.colors.text_primary),
                browser
                    .playmark_timing_editor
                    .as_ref()
                    .filter(|editor| browser.playmark_timing_editor_is_current(editor))
                    .map(|_| browser.playmark_timing_input.clone()),
                browser.ax_frame_setter("transport-playmark-timing"),
            ));
        }
    }
    if let Some(region) = edit_region {
        let edit_color = color(theme.colors.waveform_edit_selection);
        bars = bars.child(waveform_edit_region_overlay(
            region,
            browser.waveform_viewport,
            edit_color.opacity(46.0 / 255.0),
            edit_color.opacity(230.0 / 255.0),
            edit_color.opacity(180.0 / 255.0),
            edit_color.opacity(190.0 / 255.0),
            beat_guides_for_region(
                beat_guides,
                active_selection_kind,
                WaveformSelectionKind::Edit,
            ),
        ));
        bars = bars.child(waveform_edit_gain_handle_overlay(
            region,
            browser.waveform_viewport,
            edit_color.opacity(220.0 / 255.0),
        ));
        bars = bars.child(waveform_fades::render_overlay(
            region,
            edit_settings,
            browser.waveform_viewport,
            edit_color,
            browser
                .waveform_region_drag
                .is_none()
                .then_some(browser.waveform_fade_hover)
                .flatten(),
            browser.waveform_fade_render_cache.clone(),
        ));
    }
    if let Some(drag) = drag.filter(|drag| drag.is_edit_gain()) {
        let gain = drag.preview_edit_settings().gain();
        bars = bars.child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(18.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_primary))
                .child(format!("Gain {gain:.2}×")),
        );
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
    bars = bars.child(played_ranges::render_overlay(
        browser,
        color(theme.colors.waveform_played),
    ));
    let click_bounds_for_down = Rc::clone(&click_bounds);
    let click_bounds_for_secondary = Rc::clone(&click_bounds);
    let click_bounds_for_wheel = Rc::clone(&click_bounds);
    let click_bounds_for_middle = Rc::clone(&click_bounds);
    let click_bounds_for_hover = Rc::clone(&click_bounds);
    let bars = bars
        .track_focus(&browser.waveform_focus_handle)
        .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
            if view.waveform_focus_handle.is_focused(window)
                && view.handle_panel_focus_key(event, window, cx)
            {
                cx.stop_propagation();
            }
        }))
        .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
            if !*hovered {
                let changed = view.waveform_fade_hover.take().is_some();
                if similar_sections::hover(view, None) || changed {
                    cx.notify();
                }
            }
        }))
        .on_mouse_move(cx.listener(move |view, event: &MouseMoveEvent, _, cx| {
            let bounds = click_bounds_for_hover.borrow().as_ref().copied();
            let ratio = bounds
                .filter(|bounds| bounds.contains(&event.position))
                .and_then(|bounds| waveform_click_ratio(event.position, bounds))
                .map(|ratio| view.waveform_viewport.absolute_ratio_from_visible(ratio));
            if let Some(ratio) =
                ratio.filter(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio))
            {
                view.waveform_hover_cursor = Some(ratio as f32);
            }
            let similar_changed = similar_sections::hover(view, ratio);
            if view.update_waveform_fade_hover(event.position, bounds) || similar_changed {
                cx.notify();
            }
        }))
        .on_scroll_wheel(
            cx.listener(move |view, event: &ScrollWheelEvent, window, cx| {
                let bounds = click_bounds_for_wheel.borrow().as_ref().copied();
                window.focus(&view.waveform_focus_handle);
                if view.handle_waveform_wheel(event, bounds, cx) {
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
                view.cancel_waveform_gestures(window, cx);
                view.begin_waveform_pan(event.position, bounds, window, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                if view.playmark_timing_editor.is_some() {
                    view.cancel_playmark_timing_edit();
                    window.focus(&view.waveform_focus_handle);
                }
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
                if event.click_count >= 2
                    && let Some(leading) = view
                        .waveform_edit_region
                        .and_then(|region| {
                            waveform_fades::handle_at(
                                region,
                                view.waveform_edit_settings,
                                view.waveform_viewport,
                                bounds,
                                event.position,
                            )
                        })
                        .and_then(fade_silence_clear_side)
                {
                    view.cancel_waveform_gestures(window, cx);
                    view.clear_edit_fade_silence(leading);
                    view.clear_waveform_fade_hover();
                    cx.notify();
                    return;
                }
                view.cancel_waveform_gestures(window, cx);
                view.clear_waveform_fade_hover();
                view.waveform_zoom_anchor_ratio =
                    view.waveform_viewport.absolute_ratio_from_visible(ratio);
                let mut drag = begin_primary_waveform_drag_with_settings(
                    view.waveform_play_region,
                    view.waveform_edit_region,
                    view.waveform_edit_settings,
                    event.position,
                    ratio,
                    bounds,
                    view.waveform_viewport,
                    view.waveform_generation,
                );
                let modifiers = event.modifiers;
                let tempo_modifier = if cfg!(target_os = "macos") {
                    modifiers.platform
                } else {
                    modifiers.control
                };
                if tempo_modifier
                    && !modifiers.shift
                    && !modifiers.alt
                    && !modifiers.function
                    && let Some(preview) = view.waveform.as_ref()
                {
                    drag.capture_tempo_resize(
                        preview.total_frames,
                        preview.sample_rate,
                        view.beat_guide_count,
                    );
                }
                if view.bpm_snap_enabled
                    && let Some(preview) = view.waveform.as_ref()
                    && let Some(bpm) = playmark_tempo::authored_bpm(view)
                {
                    drag.capture_paint_grid(preview.total_frames, preview.sample_rate, bpm);
                }
                if matches!(drag.mode, WaveformRegionDragMode::Create { .. })
                    && let Some(preview) = view.waveform.as_ref()
                    && let Some(region) = view.waveform_play_region
                    && let Some((_, layout)) = playmark_timing::label_layout(
                        bounds,
                        view.waveform_viewport,
                        region,
                        preview.total_frames,
                        preview.sample_rate,
                        view.beat_guides_enabled.then_some(view.beat_guide_count),
                    )
                    && layout.rect.contains(&event.position)
                    && view.begin_playmark_timing_edit(window, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                window.focus(&view.waveform_focus_handle);
                if matches!(drag.mode, WaveformRegionDragMode::Create { .. })
                    && similar_sections::select_at(
                        view,
                        view.waveform_viewport.absolute_ratio_from_visible(ratio),
                        window,
                        cx,
                    )
                {
                    return;
                }
                if drag.tempo_resize.is_some() {
                    view.begin_playmark_tempo_audition();
                }
                view.waveform_region_drag = Some(drag);
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                window.focus(&view.waveform_focus_handle);
                if view.waveform.is_none() {
                    return;
                }
                let Some(bounds) = click_bounds_for_secondary.borrow().as_ref().copied() else {
                    return;
                };
                if !bounds.contains(&event.position) {
                    return;
                }
                let Some(ratio) = waveform_click_ratio(event.position, bounds) else {
                    return;
                };
                view.cancel_waveform_gestures(window, cx);
                view.clear_waveform_fade_hover();
                view.waveform_zoom_anchor_ratio =
                    view.waveform_viewport.absolute_ratio_from_visible(ratio);
                let drag = begin_secondary_waveform_drag_with_settings(
                    view.waveform_edit_region,
                    view.waveform_edit_settings,
                    event.position,
                    ratio,
                    bounds,
                    view.waveform_viewport,
                    view.waveform_generation,
                );
                if matches!(drag.mode, WaveformRegionDragMode::Create { .. })
                    && similar_sections::select_at(
                        view,
                        view.waveform_viewport.absolute_ratio_from_visible(ratio),
                        window,
                        cx,
                    )
                {
                    return;
                }
                view.waveform_region_drag = Some(drag);
                cx.notify();
            }),
        )
        .on_drag(
            WaveformDragToken(cx.entity().downgrade()),
            |token, _, window, cx| {
                // GPUI consumes the move that starts its active drag.
                let position = window.mouse_position();
                let _ = token.0.update(cx, |view, cx| {
                    if view.waveform_region_drag.is_some_and(|drag| {
                        drag.tempo_resize.is_some() || drag.paint_grid.is_some()
                    }) && view.update_waveform_region_drag(position, Some(MouseButton::Left), cx)
                    {
                        cx.notify();
                    }
                });
                cx.new(|_| WaveformDragGhost)
            },
        )
        .on_drag_move::<WaveformDragToken>(cx.listener(
            |view, event: &DragMoveEvent<WaveformDragToken>, _, cx| {
                view.update_waveform_region_drag(event.event.position, Some(MouseButton::Left), cx);
                zero_crossing::schedule_for_drag(view, cx);
                cx.notify();
            },
        ))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, _, cx| {
                view.finish_waveform_region_drag(event.position, MouseButton::Left, cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, _, cx| {
                view.finish_waveform_region_drag(event.position, MouseButton::Left, cx);
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
    pub(super) fn update_waveform_fade_hover(
        &mut self,
        position: Point<Pixels>,
        bounds: Option<Bounds<Pixels>>,
    ) -> bool {
        let next = if self.waveform_region_drag.is_none() {
            bounds
                .and_then(|bounds| self.waveform_edit_region.map(|region| (region, bounds)))
                .and_then(|(region, bounds)| {
                    waveform_fades::handle_at(
                        region,
                        self.waveform_edit_settings,
                        self.waveform_viewport,
                        bounds,
                        position,
                    )
                })
        } else {
            None
        };
        if self.waveform_fade_hover == next {
            return false;
        }
        self.waveform_fade_hover = next;
        true
    }

    fn clear_waveform_fade_hover(&mut self) {
        self.waveform_fade_hover = None;
    }

    fn clear_edit_fade_silence(&mut self, leading: bool) -> bool {
        let settings = self.waveform_edit_settings;
        let Some(fade) = (if leading {
            settings.fade_in()
        } else {
            settings.fade_out()
        }) else {
            return false;
        };
        let replacement = (fade.length() > f32::EPSILON).then(|| {
            WaveformEditFade::new(fade.length(), fade.curve(), 0.0, fade.outer_gain())
                .expect("existing finite fade remains finite when its mute is cleared")
        });
        let next = if leading {
            WaveformEditSettings::new(settings.gain(), replacement, settings.fade_out())
        } else {
            WaveformEditSettings::new(settings.gain(), settings.fade_in(), replacement)
        };
        let Some(next) = next else {
            return false;
        };
        if next == settings {
            return false;
        }
        self.waveform_edit_settings = next;
        zero_crossing::cancel_for_interruption(self);
        true
    }

    pub(super) fn clear_waveform_region(&mut self) {
        self.held_playmark_slide = None;
        self.playmark_resize_anchor = None;
        self.cancel_playmark_timing_edit();
        self.similar_sections.cancel();
        zero_crossing::cancel_for_interruption(self);
        self.waveform_play_region = None;
        self.waveform_edit_region = None;
        self.waveform_edit_settings = default_edit_settings();
        self.clear_waveform_fade_hover();
        self.waveform_region_drag = None;
        self.playmark_tempo_audition = None;
        self.waveform_pan_drag = None;
        self.waveform_scrollbar_drag = None;
    }

    pub(super) fn set_waveform_play_region(&mut self, region: Option<WaveformPlayRegion>) {
        if self.waveform_play_region != region {
            self.playmark_tempo.preserve_local_edit();
            self.held_playmark_slide = None;
            self.playmark_resize_anchor = None;
            self.cancel_playmark_timing_edit();
        }
        self.waveform_play_region = region;
    }

    pub(super) fn cancel_waveform_region_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let canceled_timing = self.cancel_playmark_timing_edit();
        let had_region_drag = self.abandon_waveform_region_drag(cx);
        if had_region_drag {
            self.clear_waveform_fade_hover();
            cx.stop_active_drag(window);
        }
        let canceled_zero_crossing = zero_crossing::cancel_for_interruption(self);
        had_region_drag || canceled_timing || canceled_zero_crossing
    }

    pub(super) fn cancel_waveform_gestures(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let canceled_timing = self.cancel_playmark_timing_edit();
        let had_region_drag = self.abandon_waveform_region_drag(cx);
        let had_pan_drag = self.waveform_pan_drag.take().is_some();
        let had_scrollbar_drag = self.waveform_scrollbar_drag.take().is_some();
        if had_region_drag {
            self.clear_waveform_fade_hover();
        }
        let canceled_zero_crossing = zero_crossing::cancel_for_interruption(self);
        let canceled = had_region_drag
            || had_pan_drag
            || had_scrollbar_drag
            || canceled_timing
            || canceled_zero_crossing;
        if canceled {
            if had_region_drag || had_pan_drag || had_scrollbar_drag {
                cx.stop_active_drag(window);
            }
        }
        canceled
    }

    pub(super) fn abandon_waveform_region_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let abandoned = self.waveform_region_drag.take().is_some();
        self.finish_playmark_tempo_audition(None, cx);
        abandoned
    }

    pub(super) fn cancel_tempo_playmark_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let canceled = self
            .waveform_region_drag
            .is_some_and(|drag| drag.tempo_resize.is_some() || drag.paint_grid.is_some())
            && self.cancel_waveform_gestures(window, cx);
        if canceled {
            cx.notify();
        }
        canceled
    }

    pub(super) fn invalidate_waveform_generation(&mut self) {
        played_ranges::clear_history(self);
        self.playmark_tempo.invalidate();
        self.transient_markers.invalidate();
        self.waveform_generation = self.waveform_generation.wrapping_add(1);
        self.waveform_hover_cursor = None;
        self.waveform_last_play_start = None;
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
        self.clear_waveform_fade_hover();
        zero_crossing::cancel_for_interruption(self);
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
        cx: &mut Context<Self>,
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
        self.cancel_playmark_timing_edit();
        self.waveform_viewport = next;
        self.abandon_waveform_region_drag(cx);
        self.waveform_pan_drag = None;
        zero_crossing::cancel_for_interruption(self);
        true
    }

    pub(super) fn zoom_waveform_to_selection(&mut self, cx: &mut Context<Self>) -> bool {
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
        self.cancel_playmark_timing_edit();
        self.waveform_viewport = next;
        self.abandon_waveform_region_drag(cx);
        self.waveform_pan_drag = None;
        zero_crossing::cancel_for_interruption(self);
        true
    }

    pub(super) fn reset_waveform_viewport(&mut self, cx: &mut Context<Self>) -> bool {
        if self.waveform.is_none() {
            return false;
        }
        let next = WaveformViewport::full();
        if self.waveform_viewport.is_effectively_equal(next) {
            return false;
        }
        self.cancel_playmark_timing_edit();
        self.waveform_viewport = next;
        self.abandon_waveform_region_drag(cx);
        self.waveform_pan_drag = None;
        zero_crossing::cancel_for_interruption(self);
        true
    }

    pub(super) fn handle_playmark_slide(
        &mut self,
        direction: WaveformRegionSlide,
        held: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fine = direction.is_fine();
        if !fine
            && held
            && (!window.is_window_active() || self.held_playmark_slide != Some(direction))
        {
            return;
        }
        let Some((region, preview)) = self.waveform_play_region.zip(self.waveform.as_ref()) else {
            self.held_playmark_slide = None;
            return;
        };
        let total_frames = preview.total_frames;
        let active_direction = (!fine).then_some(direction);
        let Some(next) = region
            .shifted_by_frames(direction, total_frames)
            .filter(|next| *next != region)
        else {
            self.held_playmark_slide = None;
            if !held && self.cancel_waveform_gestures(window, cx) {
                cx.notify();
            }
            return;
        };
        self.cancel_waveform_gestures(window, cx);
        self.set_waveform_play_region(Some(next));
        self.held_playmark_slide = active_direction;
        similar_sections::invalidate_if_stale(self);
        let viewport = self.waveform_viewport;
        let viewport_shift = if next.start < viewport.start {
            next.start - viewport.start
        } else if next.end > viewport.end {
            next.end - viewport.end
        } else {
            0.0
        };
        if viewport_shift.abs() > WAVEFORM_WHEEL_EPSILON {
            self.waveform_viewport =
                viewport.pan_by_visible_fraction(total_frames, viewport_shift / viewport.width());
        }
        if let Some(path) = self.playing_path.clone() {
            match self.submit_playmark_timing_span(&path, next, cx) {
                Some(Ok(())) if fine => {
                    self.playback_status = "Fine selection nudge applying to playback".into();
                }
                Some(Err(error)) if fine => {
                    self.playback_status = format!("Fine selection nudge unavailable: {error}");
                }
                Some(Err(error)) => {
                    self.playback_status = format!("Selection move unavailable: {error}");
                }
                _ => {}
            }
        }
        cx.notify();
    }

    pub(super) fn update_waveform_region_drag(
        &mut self,
        position: Point<Pixels>,
        pressed_button: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.waveform_region_drag else {
            return false;
        };
        if !waveform_drag_button_is_pressed(drag, pressed_button)
            || !waveform_drag_is_current(drag, self.waveform_generation, self.waveform.is_some())
        {
            self.abandon_waveform_region_drag(cx);
            self.clear_waveform_fade_hover();
            zero_crossing::cancel_for_interruption(self);
            return true;
        }
        let mut drag = drag;
        drag.update(position);
        self.waveform_region_drag = Some(drag);
        if drag.moved
            && let Some(capture) = drag.tempo_resize
        {
            let baseline = self
                .waveform_play_region
                .and_then(|span| span.frame_bounds(capture.total_frames));
            let preview = drag
                .tempo_resize_region(capture)
                .filter(|span| span.frame_bounds(capture.total_frames) != baseline);
            self.preview_playmark_tempo_span(preview, capture.beat_count, cx);
        }
        true
    }

    pub(super) fn finish_waveform_region_drag(
        &mut self,
        position: Point<Pixels>,
        button: MouseButton,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.waveform_region_drag.take() else {
            return;
        };
        self.clear_waveform_fade_hover();
        if drag.button() != button {
            self.waveform_region_drag = Some(drag);
            return;
        }
        if !waveform_drag_is_current(drag, self.waveform_generation, self.waveform.is_some()) {
            self.finish_playmark_tempo_audition(None, cx);
            return;
        }
        let mut drag = drag;
        drag.update(position);
        let bpm_snap = self.bpm_snap_settings();
        if drag.is_edit_gain() {
            if drag.moved {
                self.waveform_edit_settings = drag.preview_edit_settings();
            }
            zero_crossing::cancel_for_interruption(self);
        } else if drag.is_edit_fade() {
            if drag.moved {
                if let Some(region) = drag.preview_edit_region() {
                    self.waveform_edit_region = Some(region);
                    self.waveform_edit_settings = drag.preview_edit_settings_for_region(region);
                }
            }
            zero_crossing::cancel_for_interruption(self);
        } else if drag.selection_kind() == WaveformSelectionKind::Edit {
            if drag.moved {
                let preview = if bpm_snap.enabled {
                    drag.preview_edit_region_with_bpm(bpm_snap)
                } else {
                    drag.preview_edit_region()
                };
                if let Some(region) = preview {
                    self.waveform_edit_region = Some(region);
                    self.waveform_edit_settings = drag.preview_edit_settings_for_region(region);
                    if drag.zero_crossing_applies(bpm_snap) {
                        zero_crossing::commit_or_schedule_edit(
                            self,
                            region,
                            drag.edit_resize_effects(),
                            cx,
                        );
                    }
                }
            } else if secondary_drag_should_clear_edit(drag) {
                self.waveform_edit_region = None;
                self.waveform_edit_settings = default_edit_settings();
                zero_crossing::cancel_for_interruption(self);
            }
        } else if drag.is_handle_drag() {
            if drag.moved {
                if let Some(region) = drag.preview_region_with_bpm(bpm_snap) {
                    let tempo_span_changed = drag.tempo_resize.is_some_and(|capture| {
                        self.waveform_play_region
                            .and_then(|previous| previous.frame_bounds(capture.total_frames))
                            != region.frame_bounds(capture.total_frames)
                    });
                    if tempo_span_changed
                        && let Some(capture) = drag.tempo_resize
                        && let Some((first, end)) = region.frame_bounds(capture.total_frames)
                        && let Some(tempo) = SourcePlaymarkTempo::new(
                            first as u64,
                            end as u64,
                            capture.total_frames as u64,
                            capture.sample_rate,
                            capture.beat_count,
                        )
                        && playmark_tempo::save_authored_tempo(self, tempo, cx)
                    {
                        self.finish_playmark_tempo_audition(None, cx);
                        cx.notify();
                        return;
                    }
                    self.set_waveform_play_region(Some(region));
                    similar_sections::invalidate_if_stale(self);
                    if tempo_span_changed && let Some(capture) = drag.tempo_resize {
                        self.finish_playmark_tempo_audition(Some((region, capture.beat_count)), cx);
                    } else if drag.zero_crossing_applies(bpm_snap) {
                        zero_crossing::commit_or_schedule(self, region, cx);
                    }
                }
            }
        } else if drag.moved {
            let region = drag.preview_region();
            if drag.paint_grid.is_none() || region.is_some() {
                self.stop_playback();
            }
            if let Some(region) = region {
                self.set_waveform_play_region(Some(region));
                similar_sections::invalidate_if_stale(self);
                if drag.zero_crossing_applies(bpm_snap) {
                    zero_crossing::commit_or_schedule(self, region, cx);
                }
            }
        } else if let Some(ratio) = drag.ratio_at(position) {
            self.set_waveform_play_region(None);
            similar_sections::invalidate_if_stale(self);
            self.start_playback_from_position(ratio, cx);
        }
        self.finish_playmark_tempo_audition(None, cx);
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

#[cfg(test)]
fn begin_primary_waveform_drag(
    play_region: Option<WaveformPlayRegion>,
    edit_region: Option<WaveformEditRegion>,
    position: Point<Pixels>,
    visible_ratio: f64,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    generation: u64,
) -> WaveformRegionDrag {
    begin_primary_waveform_drag_with_settings(
        play_region,
        edit_region,
        default_edit_settings(),
        position,
        visible_ratio,
        bounds,
        viewport,
        generation,
    )
}

fn begin_primary_waveform_drag_with_settings(
    play_region: Option<WaveformPlayRegion>,
    edit_region: Option<WaveformEditRegion>,
    edit_settings: WaveformEditSettings,
    position: Point<Pixels>,
    visible_ratio: f64,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    generation: u64,
) -> WaveformRegionDrag {
    if let Some(region) = edit_region
        && let Some(hit) =
            waveform_fades::handle_at(region, edit_settings, viewport, bounds, position)
    {
        return match hit {
            waveform_fades::WaveformEditFadeHit::Standard(handle) => {
                WaveformRegionDrag::begin_edit_fade(
                    position,
                    visible_ratio,
                    bounds,
                    viewport,
                    region,
                    edit_settings,
                    generation,
                    MouseButton::Left,
                    handle,
                )
            }
            waveform_fades::WaveformEditFadeHit::OuterGain { leading } => {
                WaveformRegionDrag::begin_edit_fade_outer_gain(
                    position,
                    visible_ratio,
                    bounds,
                    viewport,
                    region,
                    edit_settings,
                    generation,
                    MouseButton::Left,
                    leading,
                )
            }
        };
    }
    if let Some(region) = edit_region
        && edit_gain_handle_at(region, viewport, bounds, position)
    {
        return WaveformRegionDrag::begin_edit_gain(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            edit_settings,
            generation,
            MouseButton::Left,
        );
    }
    if let Some(region) = edit_region
        && let Some(handle) = region_resize_handle_at(
            region.start(),
            region.end(),
            viewport,
            bounds,
            position,
            EDIT_REGION_RESIZE_HIT_WIDTH,
        )
    {
        return WaveformRegionDrag::begin_edit_handle_with_button(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            handle,
            generation,
            MouseButton::Left,
            edit_settings,
        );
    }
    if let Some(region) = play_region {
        if let Some(handle) = region_resize_handle_at(
            region.start,
            region.end,
            viewport,
            bounds,
            position,
            PLAY_REGION_RESIZE_HIT_WIDTH,
        ) {
            return WaveformRegionDrag::begin_handle(
                position,
                visible_ratio,
                bounds,
                viewport,
                region,
                handle,
                generation,
            )
            .expect("play region handles have a valid playback baseline");
        }
        if let Some(handle) = region_move_handle_at(
            region.start,
            region.end,
            viewport,
            bounds,
            position,
            PLAY_REGION_MOVE_GRIP_HEIGHT,
            PLAY_REGION_MOVE_GRIP_INSET,
        ) {
            return WaveformRegionDrag::begin_handle(
                position,
                visible_ratio,
                bounds,
                viewport,
                region,
                handle,
                generation,
            )
            .expect("play region handles have a valid playback baseline");
        }
    }
    if let Some(region) = edit_region
        && let Some(handle) = region_move_handle_at(
            region.start(),
            region.end(),
            viewport,
            bounds,
            position,
            EDIT_REGION_MOVE_GRIP_HEIGHT,
            EDIT_REGION_MOVE_GRIP_INSET,
        )
    {
        return WaveformRegionDrag::begin_edit_handle_with_button(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            handle,
            generation,
            MouseButton::Left,
            edit_settings,
        );
    }
    WaveformRegionDrag::begin(position, visible_ratio, bounds, viewport, generation)
}

#[cfg(test)]
fn begin_secondary_waveform_drag(
    edit_region: Option<WaveformEditRegion>,
    position: Point<Pixels>,
    visible_ratio: f64,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    generation: u64,
) -> WaveformRegionDrag {
    begin_secondary_waveform_drag_with_settings(
        edit_region,
        default_edit_settings(),
        position,
        visible_ratio,
        bounds,
        viewport,
        generation,
    )
}

fn begin_secondary_waveform_drag_with_settings(
    edit_region: Option<WaveformEditRegion>,
    edit_settings: WaveformEditSettings,
    position: Point<Pixels>,
    visible_ratio: f64,
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    generation: u64,
) -> WaveformRegionDrag {
    if let Some(region) = edit_region
        && let Some(hit) =
            waveform_fades::handle_at(region, edit_settings, viewport, bounds, position)
    {
        return match hit {
            waveform_fades::WaveformEditFadeHit::Standard(handle) => {
                WaveformRegionDrag::begin_edit_fade(
                    position,
                    visible_ratio,
                    bounds,
                    viewport,
                    region,
                    edit_settings,
                    generation,
                    MouseButton::Right,
                    handle,
                )
            }
            waveform_fades::WaveformEditFadeHit::OuterGain { leading } => {
                WaveformRegionDrag::begin_edit_fade_outer_gain(
                    position,
                    visible_ratio,
                    bounds,
                    viewport,
                    region,
                    edit_settings,
                    generation,
                    MouseButton::Right,
                    leading,
                )
            }
        };
    }
    if let Some(region) = edit_region
        && edit_gain_handle_at(region, viewport, bounds, position)
    {
        return WaveformRegionDrag::begin_edit_gain(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            edit_settings,
            generation,
            MouseButton::Right,
        );
    }
    if let Some(region) = edit_region
        && let Some(handle) = edit_region_handle_at(region, viewport, bounds, position)
    {
        return WaveformRegionDrag::begin_edit_handle_with_button(
            position,
            visible_ratio,
            bounds,
            viewport,
            region,
            handle,
            generation,
            MouseButton::Right,
            edit_settings,
        );
    }
    WaveformRegionDrag::begin_edit(position, visible_ratio, bounds, viewport, generation)
}

#[cfg(test)]
fn waveform_region_handle_at(
    region: WaveformPlayRegion,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<WaveformRegionHandle> {
    region_handle_at(
        region.start,
        region.end,
        viewport,
        bounds,
        position,
        PLAY_REGION_RESIZE_HIT_WIDTH,
        PLAY_REGION_MOVE_GRIP_HEIGHT,
        PLAY_REGION_MOVE_GRIP_INSET,
    )
}

fn edit_region_handle_at(
    region: WaveformEditRegion,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<WaveformRegionHandle> {
    region_handle_at(
        region.start(),
        region.end(),
        viewport,
        bounds,
        position,
        EDIT_REGION_RESIZE_HIT_WIDTH,
        EDIT_REGION_MOVE_GRIP_HEIGHT,
        EDIT_REGION_MOVE_GRIP_INSET,
    )
}

fn edit_gain_handle_at(
    region: WaveformEditRegion,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> bool {
    edit_gain_handle_rect(
        region,
        viewport,
        bounds,
        EDIT_GAIN_HANDLE_HIT_SIZE,
        EDIT_GAIN_HANDLE_HIT_SIZE,
    )
    .is_some_and(|rect| rect.contains(&position))
}

fn edit_gain_handle_rect(
    region: WaveformEditRegion,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    width: f32,
    height: f32,
) -> Option<Bounds<Pixels>> {
    if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
        return None;
    }
    let Some((visible_start, visible_end)) =
        viewport.visible_range_from_absolute(region.start(), region.end())
    else {
        return None;
    };
    let bounds_width = f32::from(bounds.size.width);
    let bounds_height = f32::from(bounds.size.height);
    let bounds_left = f32::from(bounds.origin.x);
    let bounds_top = f32::from(bounds.origin.y);
    if !bounds_width.is_finite()
        || bounds_width <= 0.0
        || !bounds_height.is_finite()
        || bounds_height <= 0.0
        || !bounds_left.is_finite()
        || !bounds_top.is_finite()
    {
        return None;
    }
    let width = width.min(bounds_width);
    let height = height.min(bounds_height);
    let center = bounds_left + bounds_width * ((visible_start + visible_end) * 0.5) as f32;
    let left = (center - width * 0.5).clamp(bounds_left, bounds_left + bounds_width - width);
    Some(Bounds::from_corners(
        point(px(left), px(bounds_top)),
        point(px(left + width), px(bounds_top + height)),
    ))
}

fn region_resize_handle_at(
    start: f64,
    end: f64,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    resize_hit_width: f32,
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
    if y < top || y > top + PLAY_REGION_RESIZE_HANDLE_HEIGHT {
        return None;
    }
    let start_x = viewport
        .visible_ratio_from_absolute(start)
        .map(|ratio| left + width * ratio as f32);
    let end_x = viewport
        .visible_ratio_from_absolute(end)
        .map(|ratio| left + width * ratio as f32);
    let half_hit_width = resize_hit_width * 0.5;
    let start_distance = start_x
        .filter(|center| (x - center).abs() <= half_hit_width)
        .map(|center| (x - center).abs());
    let end_distance = end_x
        .filter(|center| (x - center).abs() <= half_hit_width)
        .map(|center| (x - center).abs());
    match (start_distance, end_distance) {
        (Some(start), Some(end)) if end < start => Some(WaveformRegionHandle::ResizeEnd),
        (Some(_), Some(_)) | (Some(_), None) => Some(WaveformRegionHandle::ResizeStart),
        (None, Some(_)) => Some(WaveformRegionHandle::ResizeEnd),
        (None, None) => None,
    }
}

fn region_move_handle_at(
    start: f64,
    end: f64,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    move_grip_height: f32,
    move_grip_inset: f32,
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
    if y > top + move_grip_height {
        return None;
    }
    let Some((visible_start, visible_end)) = viewport.visible_range_from_absolute(start, end)
    else {
        return None;
    };
    let grip_left = left + width * visible_start as f32 + move_grip_inset;
    let grip_right = left + width * visible_end as f32 - move_grip_inset;
    (grip_right > grip_left && x >= grip_left && x <= grip_right)
        .then_some(WaveformRegionHandle::Move)
}

fn region_handle_at(
    start: f64,
    end: f64,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    resize_hit_width: f32,
    move_grip_height: f32,
    move_grip_inset: f32,
) -> Option<WaveformRegionHandle> {
    region_resize_handle_at(start, end, viewport, bounds, position, resize_hit_width).or_else(
        || {
            region_move_handle_at(
                start,
                end,
                viewport,
                bounds,
                position,
                move_grip_height,
                move_grip_inset,
            )
        },
    )
}

fn waveform_drag_is_current(
    drag: WaveformRegionDrag,
    generation: u64,
    waveform_loaded: bool,
) -> bool {
    waveform_loaded && drag.generation == generation
}

fn waveform_drag_button_is_pressed(
    drag: WaveformRegionDrag,
    pressed_button: Option<MouseButton>,
) -> bool {
    pressed_button == Some(drag.button())
}

fn secondary_drag_should_clear_edit(drag: WaveformRegionDrag) -> bool {
    drag.selection_kind() == WaveformSelectionKind::Edit
        && drag.button() == MouseButton::Right
        && !drag.is_edit_effect_handle()
        && !drag.moved
}

fn fade_silence_clear_side(hit: waveform_fades::WaveformEditFadeHit) -> Option<bool> {
    match hit {
        waveform_fades::WaveformEditFadeHit::Standard(WaveformEditFadeHandle::InOuterStart) => {
            Some(true)
        }
        waveform_fades::WaveformEditFadeHit::Standard(WaveformEditFadeHandle::OutOuterEnd) => {
            Some(false)
        }
        waveform_fades::WaveformEditFadeHit::Standard(_)
        | waveform_fades::WaveformEditFadeHit::OuterGain { .. } => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveformRegionSlide {
    Left,
    Right,
    FineLeft,
    FineRight,
}

impl WaveformRegionSlide {
    pub(super) fn is_fine(self) -> bool {
        matches!(self, Self::FineLeft | Self::FineRight)
    }
}

pub(super) fn waveform_region_navigation_key(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> Option<WaveformRegionSlide> {
    if secondary || function || (shift && alt) {
        return None;
    }
    match (key, shift || alt) {
        ("left" | "arrowleft", false) => Some(WaveformRegionSlide::Left),
        ("right" | "arrowright", false) => Some(WaveformRegionSlide::Right),
        ("left" | "arrowleft", true) => Some(WaveformRegionSlide::FineLeft),
        ("right" | "arrowright", true) => Some(WaveformRegionSlide::FineRight),
        _ => None,
    }
}

fn waveform_region_overlay(
    region: WaveformPlayRegion,
    viewport: WaveformViewport,
    selection_color: gpui::Hsla,
    edge_color: gpui::Hsla,
    beat_guides: Option<(u8, gpui::Hsla)>,
) -> impl IntoElement {
    waveform_selection_overlay(
        region.start,
        region.end,
        viewport,
        selection_color,
        edge_color,
        edge_color.opacity(0.75),
        edge_color.opacity(0.9),
        PLAY_REGION_RESIZE_HANDLE_WIDTH,
        PLAY_REGION_RESIZE_HANDLE_HEIGHT,
        PLAY_REGION_MOVE_GRIP_HEIGHT,
        PLAY_REGION_MOVE_GRIP_INSET,
        beat_guides,
    )
}

fn beat_guides_for_region(
    beat_guides: Option<(u8, gpui::Hsla)>,
    active_selection_kind: Option<WaveformSelectionKind>,
    region_kind: WaveformSelectionKind,
) -> Option<(u8, gpui::Hsla)> {
    match active_selection_kind {
        Some(active_kind) if active_kind != region_kind => None,
        _ => beat_guides,
    }
}

fn waveform_edit_region_overlay(
    region: WaveformEditRegion,
    viewport: WaveformViewport,
    selection_color: gpui::Hsla,
    edge_color: gpui::Hsla,
    grip_color: gpui::Hsla,
    handle_color: gpui::Hsla,
    beat_guides: Option<(u8, gpui::Hsla)>,
) -> impl IntoElement {
    waveform_selection_overlay(
        region.start(),
        region.end(),
        viewport,
        selection_color,
        edge_color,
        grip_color,
        handle_color,
        EDIT_REGION_RESIZE_HANDLE_WIDTH,
        EDIT_REGION_RESIZE_HANDLE_HEIGHT,
        EDIT_REGION_MOVE_GRIP_HEIGHT,
        EDIT_REGION_MOVE_GRIP_INSET,
        beat_guides,
    )
}

fn waveform_edit_gain_handle_overlay(
    region: WaveformEditRegion,
    viewport: WaveformViewport,
    handle_color: gpui::Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let Some(handle) = edit_gain_handle_rect(
                region,
                viewport,
                bounds,
                EDIT_GAIN_HANDLE_WIDTH,
                EDIT_GAIN_HANDLE_HEIGHT,
            ) else {
                return;
            };
            window.paint_quad(fill(handle, handle_color));
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

fn waveform_selection_overlay(
    start_ratio: f64,
    end_ratio: f64,
    viewport: WaveformViewport,
    selection_color: gpui::Hsla,
    edge_color: gpui::Hsla,
    grip_color: gpui::Hsla,
    handle_color: gpui::Hsla,
    resize_handle_width: f32,
    resize_handle_height: f32,
    move_grip_height: f32,
    move_grip_inset: f32,
    beat_guides: Option<(u8, gpui::Hsla)>,
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
                viewport.visible_range_from_absolute(start_ratio, end_ratio)
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
            if let Some((beat_count, guide_color)) = beat_guides {
                let guide_width = BEAT_GUIDE_WIDTH.min(width);
                let guide_height = (height * BEAT_GUIDE_HEIGHT_FRACTION)
                    .round()
                    .clamp(guide_width.min(height), height);
                let guide_top = (height - guide_height) * 0.5;
                for visible_ratio in
                    beat_guide_projection(start_ratio, end_ratio, viewport, beat_count)
                {
                    let center = width * visible_ratio as f32;
                    let left =
                        (center - guide_width * 0.5).clamp(0.0, (width - guide_width).max(0.0));
                    let guide_bounds = Bounds::from_corners(
                        point(bounds.left() + px(left), bounds.top() + px(guide_top)),
                        point(
                            bounds.left() + px(left + guide_width),
                            bounds.top() + px(guide_top + guide_height),
                        ),
                    );
                    window.paint_quad(fill(guide_bounds, guide_color));
                }
            }
            let edge_width = WAVEFORM_SELECTION_EDGE_WIDTH.min(width);
            for absolute in [start_ratio, end_ratio] {
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
                .visible_ratio_from_absolute(start_ratio)
                .unwrap_or(visible_start) as f32;
            let move_right = viewport
                .visible_ratio_from_absolute(end_ratio)
                .unwrap_or(visible_end) as f32;
            let grip_left = width * move_left + move_grip_inset;
            let grip_right = width * move_right - move_grip_inset;
            let grip_height = move_grip_height.min(height);
            if grip_right > grip_left && grip_height > 0.0 {
                let grip_bounds = Bounds::from_corners(
                    point(bounds.left() + px(grip_left), bounds.top()),
                    point(
                        bounds.left() + px(grip_right),
                        bounds.top() + px(grip_height),
                    ),
                );
                window.paint_quad(fill(grip_bounds, grip_color));
            }
            let handle_height = resize_handle_height.min(height);
            if handle_height > 0.0 {
                for absolute in [start_ratio, end_ratio] {
                    let Some(visible) = viewport.visible_ratio_from_absolute(absolute) else {
                        continue;
                    };
                    let center = width * visible as f32;
                    let handle_width = resize_handle_width.min(width);
                    let left = (center - handle_width * 0.5).clamp(0.0, width - handle_width);
                    let handle_bounds = Bounds::from_corners(
                        point(bounds.left() + px(left), bounds.top()),
                        point(
                            bounds.left() + px(left + handle_width),
                            bounds.top() + px(handle_height),
                        ),
                    );
                    window.paint_quad(fill(handle_bounds, handle_color));
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

    fn playback_frames(region: WaveformPlayRegion, total_frames: usize) -> (usize, usize) {
        let total = total_frames as f64;
        (
            (region.start * total).round() as usize,
            (region.end * total).round() as usize,
        )
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
    fn zoomed_peak_preview_maps_gain_to_visible_columns() {
        let viewport = WaveformViewport {
            start: 0.5,
            end: 1.0,
        };
        let visible_peaks = visible_peak_values(&[1.0; 8], viewport);
        let edit = Some((
            WaveformEditRegion::new(0.75, 0.9).unwrap(),
            WaveformEditSettings::new(0.0, None, None).unwrap(),
        ));
        let centers = visible_peaks
            .iter()
            .enumerate()
            .map(|(index, _)| gain_for_visible_peak(index, &visible_peaks, viewport, edit))
            .collect::<Vec<_>>();

        assert_eq!(visible_peaks.len(), 4);
        assert_eq!(centers, vec![None, None, Some(0.0), None]);
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
    fn cache_hits_share_vertices_and_retained_geometry_survives_invalidation() {
        let cache = WaveformRenderCache::default();
        let bands = [[0.5, 0.4, 0.3, 0.6]; 32];
        let bounds = test_bounds(320.0, 180.0);
        let viewport = WaveformViewport::full();
        let selection = WaveformEditRegion::new(0.25, 0.75).unwrap();
        let first = cache.paths_for(1, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0);
        let second = cache.paths_for(1, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0);
        assert!(Rc::ptr_eq(&first, &second));
        let preview = |gain| {
            let WaveformGainPreviewGeometry::Bands { paths, .. } = cache
                .gain_preview_paths_for(
                    1,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    gain,
                )
                .unwrap();
            paths
        };
        let before = preview(0.5);
        assert!(Rc::ptr_eq(&before, &preview(0.5)));
        assert!(!Rc::ptr_eq(&before, &preview(2.0)));
        assert_eq!(before.len(), 4);
        assert!(Rc::ptr_eq(
            &first,
            &cache.paths_for(1, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0)
        ));
    }

    #[test]
    fn gain_preview_reuses_static_and_dynamic_cache_entries() {
        let cache = WaveformRenderCache::default();
        let bounds = test_bounds(320.0, 180.0);
        let viewport = WaveformViewport::full();
        let bands = vec![[0.5, 0.4, 0.3, 0.6]; 2_048];
        let selection = WaveformEditRegion::new(0.25, 0.75).expect("edit selection");

        assert_eq!(
            cache
                .paths_for(7, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0)
                .len(),
            4
        );
        assert_eq!(cache.rebuild_count(), 1);
        assert!(
            cache
                .gain_preview_paths_for(
                    7,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    0.0,
                )
                .is_some()
        );
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.gain_preview_rebuild_count(), 1);
        assert!(
            cache
                .gain_preview_paths_for(
                    7,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    0.0,
                )
                .is_some()
        );
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.gain_preview_rebuild_count(), 1);
        assert!(
            cache
                .gain_preview_paths_for(
                    7,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    2.0,
                )
                .is_some()
        );
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.gain_preview_rebuild_count(), 2);
    }

    #[test]
    fn gain_preview_applies_gain_before_nonlinear_waveform_shaping() {
        let bands = vec![[0.2, 0.25, 0.3, 0.35]; 8];
        let selection = WaveformEditRegion::new(0.25, 0.75).expect("edit selection");
        let boosted = resample_gain_preview_extents(
            &bands,
            8,
            1.0,
            WaveformViewport::full(),
            (0.0, 1.0),
            selection,
            0.25,
            0.75,
            2.0,
        );
        let silent = resample_gain_preview_extents(
            &bands,
            8,
            1.0,
            WaveformViewport::full(),
            (0.0, 1.0),
            selection,
            0.25,
            0.75,
            0.0,
        );
        assert!(
            silent
                .iter()
                .all(|column| column.iter().all(|value| *value == 0.0))
        );
        let expected = waveform_extent([0.4, 0.5, 0.6, 0.7], 0, 1.0);
        assert!(boosted[2][0] > expected * 0.99);
        assert!(boosted[2][0] <= 1.0);
    }

    #[test]
    fn fade_preview_cache_keys_envelope_even_when_clipped_bounds_are_unchanged() {
        let cache = WaveformRenderCache::default();
        let bands = [[0.5; 4]; 64];
        let viewport = WaveformViewport {
            start: 0.4,
            end: 0.6,
        };
        let region = WaveformEditRegion::new(0.25, 0.75).unwrap();
        let preview = |curve| {
            let fade = yield_core::WaveformEditFade::new(0.5, curve, 0.0, 1.0).unwrap();
            let settings = WaveformEditSettings::new(1.0, Some(fade), None).unwrap();
            let WaveformGainPreviewGeometry::Bands { paths, .. } = cache
                .edit_preview_paths_for(
                    1,
                    &bands,
                    1.0,
                    test_bounds(320.0, 180.0),
                    viewport,
                    (0.0, 1.0),
                    0,
                    region,
                    settings,
                )
                .unwrap();
            paths
        };
        let linear = preview(0.0);
        assert!(Rc::ptr_eq(&linear, &preview(0.0)));
        let curved = preview(1.0);
        assert!(!Rc::ptr_eq(&linear, &curved));
        assert!(Rc::ptr_eq(&curved, &preview(1.0)));
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(cache.gain_preview_rebuild_count(), 2);
    }

    #[test]
    fn fade_preview_shapes_signal_and_outer_spans_in_absolute_coordinates() {
        let region = WaveformEditRegion::new(0.25, 0.75).unwrap();
        let fade = yield_core::WaveformEditFade::new(0.5, 0.0, 0.5, 0.25).unwrap();
        let settings = WaveformEditSettings::new(1.0, Some(fade), None).unwrap();
        let bounds = edit_effect_bounds(region, settings);
        assert_eq!(bounds.start(), 0.0);
        assert_eq!(bounds.end(), 0.75);
        let bands = vec![[0.5; 4]; 64];
        let extents = resample_edit_preview_extents(
            &bands,
            64,
            1.0,
            WaveformViewport::full(),
            (0.0, 1.0),
            region,
            0.0,
            0.75,
            settings,
        );
        for (index, extent) in extents.iter().enumerate() {
            let ratio = 0.75 * index as f64 / (extents.len() - 1) as f64;
            let gain = settings.visual_gain_at_ratio(region, ratio);
            for (lane, actual) in extent.iter().enumerate() {
                let expected = waveform_extent([0.5 * gain; 4], lane, 1.0);
                assert!((actual - expected).abs() < 1e-6);
            }
        }
        let viewport = WaveformViewport {
            start: 0.0,
            end: 0.5,
        };
        let peaks = vec![0.5; 8];
        for index in 0..peaks.len() {
            let ratio = 0.5 * (index as f64 + 0.5) / peaks.len() as f64;
            assert_eq!(
                gain_for_visible_peak(index, &peaks, viewport, Some((region, settings))),
                Some(settings.visual_gain_at_ratio(region, ratio))
            );
        }
    }

    #[test]
    fn narrow_nonzero_preview_has_signal_at_both_local_endpoints() {
        let region = WaveformEditRegion::new(0.50001, 0.50002).unwrap();
        let extents = resample_gain_preview_extents(
            &[[0.5; 4]; 2048],
            320,
            1.0,
            WaveformViewport::full(),
            (0.0, 1.0),
            region,
            region.start(),
            region.end(),
            0.5,
        );
        assert_eq!(extents.len(), 2);
        assert!(extents.iter().all(|column| column[0] > 0.0));
    }

    #[test]
    fn gain_preview_keeps_a_narrow_selection_visible_at_zero_gain() {
        let bounds = test_bounds(320.0, 180.0);
        let viewport = WaveformViewport::full();
        let bands = vec![[0.6, 0.5, 0.4, 0.7]; 2_048];
        let selection = WaveformEditRegion::new(0.50001, 0.50002).expect("narrow selection");
        let preview = WaveformRenderCache::default()
            .gain_preview_paths_for(
                9,
                &bands,
                1.0,
                bounds,
                viewport,
                (0.0, 1.0),
                0,
                selection,
                0.0,
            )
            .expect("narrow visible preview");
        let WaveformGainPreviewGeometry::Bands {
            selection_bounds,
            paths,
        } = preview;
        assert!(selection_bounds.size.width > px(0.0));
        assert_eq!(paths.len(), 4);
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
    fn edit_region_hit_zones_use_the_tall_resize_strip_and_short_move_grip() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport {
            start: -0.5,
            end: 1.5,
        };
        let region = WaveformEditRegion::new(-0.2, 1.2).expect("edit region");

        assert_eq!(
            edit_region_handle_at(region, viewport, bounds, point(px(30.0), px(20.0))),
            Some(WaveformRegionHandle::ResizeStart)
        );
        assert_eq!(
            edit_region_handle_at(region, viewport, bounds, point(px(170.0), px(20.0))),
            Some(WaveformRegionHandle::ResizeEnd)
        );
        assert_eq!(
            edit_region_handle_at(region, viewport, bounds, point(px(100.0), px(3.0))),
            Some(WaveformRegionHandle::Move)
        );
        assert_eq!(
            edit_region_handle_at(region, viewport, bounds, point(px(100.0), px(8.0))),
            None
        );
        assert_eq!(
            edit_region_handle_at(region, viewport, bounds, point(px(100.0), px(24.0))),
            None
        );
    }

    #[test]
    fn edit_gain_handle_matches_native_size_and_visible_clipped_center() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let region = WaveformEditRegion::new(0.25, 0.75).expect("edit region");
        let visual = edit_gain_handle_rect(
            region,
            viewport,
            bounds,
            EDIT_GAIN_HANDLE_WIDTH,
            EDIT_GAIN_HANDLE_HEIGHT,
        )
        .expect("visible gain handle");
        assert!((f32::from(visual.origin.x) - 94.0).abs() < 1.0e-6);
        assert!((f32::from(visual.origin.y) - 0.0).abs() < 1.0e-6);
        assert!((f32::from(visual.size.width) - 12.0).abs() < 1.0e-6);
        assert!((f32::from(visual.size.height) - 10.0).abs() < 1.0e-6);

        let clipped = WaveformEditRegion::new(-0.2, 0.4).expect("clipped edit region");
        let clipped_visual = edit_gain_handle_rect(
            clipped,
            viewport,
            bounds,
            EDIT_GAIN_HANDLE_WIDTH,
            EDIT_GAIN_HANDLE_HEIGHT,
        )
        .expect("clipped gain handle");
        assert!((f32::from(clipped_visual.origin.x) - 34.0).abs() < 1.0e-6);
        assert!(edit_gain_handle_at(
            region,
            viewport,
            bounds,
            point(px(100.0), px(17.0))
        ));
        assert!(!edit_gain_handle_at(
            region,
            viewport,
            bounds,
            point(px(100.0), px(19.0))
        ));
        assert!(
            edit_gain_handle_rect(
                WaveformEditRegion::new(1.1, 1.2).expect("offscreen edit region"),
                viewport,
                bounds,
                EDIT_GAIN_HANDLE_WIDTH,
                EDIT_GAIN_HANDLE_HEIGHT,
            )
            .is_none()
        );
    }

    #[test]
    fn primary_hit_priority_keeps_play_and_edit_gestures_independent() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let play = WaveformPlayRegion::new(0.3, 0.7).expect("play region");
        let edit = WaveformEditRegion::new(0.2, 0.8).expect("edit region");

        let edit_resize = begin_primary_waveform_drag(
            Some(play),
            Some(edit),
            point(px(40.0), px(10.0)),
            0.2,
            bounds,
            viewport,
            1,
        );
        assert_eq!(edit_resize.selection_kind(), WaveformSelectionKind::Edit);
        assert_eq!(edit_resize.button(), MouseButton::Left);

        let play_resize = begin_primary_waveform_drag(
            Some(play),
            Some(edit),
            point(px(60.0), px(10.0)),
            0.3,
            bounds,
            viewport,
            1,
        );
        assert_eq!(play_resize.selection_kind(), WaveformSelectionKind::Play);
        assert_eq!(play_resize.button(), MouseButton::Left);

        let play_move = begin_primary_waveform_drag(
            Some(play),
            Some(edit),
            point(px(125.0), px(3.0)),
            0.625,
            bounds,
            viewport,
            1,
        );
        assert_eq!(play_move.selection_kind(), WaveformSelectionKind::Play);

        let edit_move = begin_primary_waveform_drag(
            Some(play),
            Some(edit),
            point(px(50.0), px(3.0)),
            0.25,
            bounds,
            viewport,
            1,
        );
        assert_eq!(edit_move.selection_kind(), WaveformSelectionKind::Edit);

        let play_create = begin_primary_waveform_drag(
            Some(play),
            Some(edit),
            point(px(190.0), px(50.0)),
            0.95,
            bounds,
            viewport,
            1,
        );
        assert_eq!(play_create.selection_kind(), WaveformSelectionKind::Play);
    }

    #[test]
    fn gain_handle_has_priority_for_both_pointer_buttons() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let play = WaveformPlayRegion::new(0.3, 0.7).expect("play region");
        let edit = WaveformEditRegion::new(0.2, 0.8).expect("edit region");
        let settings = WaveformEditSettings::new(1.0, None, None).expect("settings");

        let primary = begin_primary_waveform_drag_with_settings(
            Some(play),
            Some(edit),
            settings,
            point(px(100.0), px(8.0)),
            0.5,
            bounds,
            viewport,
            1,
        );
        assert!(primary.is_edit_gain());
        assert_eq!(primary.selection_kind(), WaveformSelectionKind::Edit);
        assert_eq!(primary.button(), MouseButton::Left);

        let secondary = begin_secondary_waveform_drag_with_settings(
            Some(edit),
            settings,
            point(px(100.0), px(8.0)),
            0.5,
            bounds,
            viewport,
            1,
        );
        assert!(secondary.is_edit_gain());
        assert_eq!(secondary.button(), MouseButton::Right);
    }

    #[test]
    fn fade_handle_priority_admits_all_standard_handles_and_separates_outer_gain() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let region = WaveformEditRegion::new(0.2, 0.8).expect("edit region");
        let fade_in = WaveformEditFade::new(0.25, 0.5, 0.1, 1.0).expect("fade in");
        let fade_out = WaveformEditFade::new(0.3, 0.5, 0.1, 1.0).expect("fade out");
        let settings =
            WaveformEditSettings::new(1.0, Some(fade_in), Some(fade_out)).expect("settings");
        let points = [
            (WaveformEditFadeHandle::InEnd, point(px(70.0), px(5.0))),
            (WaveformEditFadeHandle::OutStart, point(px(124.0), px(5.0))),
            (WaveformEditFadeHandle::InStart, point(px(40.0), px(95.0))),
            (WaveformEditFadeHandle::OutEnd, point(px(160.0), px(95.0))),
            (
                WaveformEditFadeHandle::InOuterStart,
                point(px(28.0), px(45.0)),
            ),
            (
                WaveformEditFadeHandle::OutOuterEnd,
                point(px(172.0), px(45.0)),
            ),
        ];
        for (handle, position) in points {
            let drag = begin_primary_waveform_drag_with_settings(
                None,
                Some(region),
                settings,
                position,
                f64::from(f32::from(position.x)) / 200.0,
                bounds,
                viewport,
                1,
            );
            assert!(drag.is_edit_fade());
            assert_eq!(drag.button(), MouseButton::Left);
            assert_eq!(drag.preview_edit_region(), Some(region));
            assert_eq!(
                drag.mode,
                WaveformRegionDragMode::EditFade {
                    baseline: region,
                    drag: WaveformEditFadeDrag::new(handle, region, settings),
                }
            );
        }

        let overlapping = WaveformEditSettings::new(
            1.0,
            Some(WaveformEditFade::new(0.25, 0.5, 0.1, 0.0).unwrap()),
            Some(fade_out),
        )
        .unwrap();
        let outer_gain = waveform_fades::handle_at(
            region,
            overlapping,
            viewport,
            bounds,
            point(px(28.0), px(50.0)),
        )
        .expect("outer gain hit");
        assert_eq!(
            outer_gain,
            waveform_fades::WaveformEditFadeHit::OuterGain { leading: true }
        );
        assert_eq!(fade_silence_clear_side(outer_gain), None);
        assert_eq!(
            fade_silence_clear_side(waveform_fades::WaveformEditFadeHit::Standard(
                WaveformEditFadeHandle::InOuterStart,
            )),
            Some(true)
        );
    }

    #[test]
    fn outer_gain_drag_changes_only_the_selected_fade_and_preserves_region() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let region = WaveformEditRegion::new(0.25, 0.75).expect("edit region");
        let fade_in = WaveformEditFade::new(0.2, 0.5, 0.25, 0.0).expect("fade in");
        let fade_out = WaveformEditFade::new(0.15, 0.2, 0.1, 0.8).expect("fade out");
        let settings =
            WaveformEditSettings::new(2.0, Some(fade_in), Some(fade_out)).expect("settings");
        let mut drag = WaveformRegionDrag::begin_edit_fade_outer_gain(
            point(px(25.0), px(50.0)),
            0.125,
            bounds,
            viewport,
            region,
            settings,
            1,
            MouseButton::Right,
            true,
        );
        drag.update(point(px(25.0), px(20.0)));
        assert!(drag.moved);
        assert_eq!(drag.preview_edit_region(), Some(region));
        let preview = drag.preview_edit_settings();
        assert!((preview.fade_in().unwrap().outer_gain() - 0.6).abs() < 1.0e-6);
        assert_eq!(preview.fade_out(), Some(fade_out));
        assert_eq!(preview.gain(), settings.gain());
        assert!(!drag.zero_crossing_applies(BpmSnapSettings::disabled()));
    }

    #[test]
    fn gain_drag_uses_one_baseline_for_asymmetric_vertical_updates() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let region = WaveformEditRegion::new(0.2, 0.8).expect("edit region");
        let fade = yield_core::WaveformEditFade::new(0.2, 0.3, 0.4, 0.5).expect("fade settings");
        let settings = WaveformEditSettings::new(1.0, Some(fade), None).expect("settings");
        let mut drag = WaveformRegionDrag::begin_edit_gain(
            point(px(100.0), px(100.0)),
            0.5,
            bounds,
            viewport,
            region,
            settings,
            1,
            MouseButton::Left,
        );

        drag.update(point(px(100.0), px(60.0)));
        assert!(drag.moved);
        assert!((drag.preview_edit_settings().gain() - 2.0).abs() < 1.0e-6);
        assert_eq!(drag.preview_edit_settings().fade_in(), Some(fade));
        assert_eq!(drag.preview_edit_region(), Some(region));
        assert!(!drag.zero_crossing_applies(BpmSnapSettings::disabled()));

        drag.update(point(px(100.0), px(140.0)));
        assert!((drag.preview_edit_settings().gain() - (2.0 / 3.0)).abs() < 1.0e-6);
        assert_eq!(settings.gain(), 1.0);
    }

    #[test]
    fn secondary_drag_preserves_extended_edit_baseline_and_uses_right_button() {
        let bounds = test_bounds(200.0, 100.0);
        let viewport = WaveformViewport {
            start: -0.5,
            end: 1.5,
        };
        let baseline = WaveformEditRegion::new(-0.2, 0.4).expect("edit region");
        let mut drag = WaveformRegionDrag::begin_edit_handle(
            point(px(30.0), px(3.0)),
            0.15,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::Move,
            1,
        );
        assert_eq!(drag.selection_kind(), WaveformSelectionKind::Edit);
        assert_eq!(drag.button(), MouseButton::Right);

        drag.update(point(px(10.0), px(3.0)));
        let moved = drag.preview_edit_region().expect("moved edit region");
        assert!((moved.start() + 0.4).abs() < 1.0e-6);
        assert!((moved.end() - 0.2).abs() < 1.0e-6);
        assert_eq!(baseline, WaveformEditRegion::new(-0.2, 0.4).unwrap());
        assert!(drag.moved);

        let create = begin_secondary_waveform_drag(
            Some(baseline),
            point(px(180.0), px(50.0)),
            0.9,
            bounds,
            viewport,
            1,
        );
        assert_eq!(create.selection_kind(), WaveformSelectionKind::Edit);
        assert_eq!(create.button(), MouseButton::Right);
        assert!(secondary_drag_should_clear_edit(create));
    }

    #[test]
    fn secondary_drag_button_loss_cancels_without_synthesizing_release() {
        let drag = WaveformRegionDrag::begin_edit(
            point(px(40.0), px(10.0)),
            0.2,
            test_bounds(200.0, 100.0),
            WaveformViewport::full(),
            5,
        );
        assert!(waveform_drag_button_is_pressed(
            drag,
            Some(MouseButton::Right)
        ));
        assert!(!waveform_drag_button_is_pressed(
            drag,
            Some(MouseButton::Left)
        ));
        assert!(!waveform_drag_button_is_pressed(drag, None));
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
    fn tempo_resize_keeps_the_opposite_frame_and_skips_old_grid_snapping() {
        let bounds = test_bounds(1000.0, 100.0);
        let baseline = WaveformPlayRegion::new(0.25, 0.65).unwrap();
        let snap = BpmSnapSettings {
            enabled: true,
            beat_count: 12,
            audio_duration_seconds: 4.0,
            constrain_to_audio: true,
        };
        for (handle, origin, target, expected) in [
            (
                WaveformRegionHandle::ResizeStart,
                0.25,
                0.15,
                (28_800, 124_800),
            ),
            (
                WaveformRegionHandle::ResizeEnd,
                0.65,
                0.75,
                (48_000, 144_000),
            ),
        ] {
            let mut drag = WaveformRegionDrag::begin_handle(
                point(px((origin * 1000.0) as f32), px(50.0)),
                origin,
                bounds,
                WaveformViewport::full(),
                baseline,
                handle,
                1,
            )
            .unwrap();
            drag.capture_tempo_resize(192_000, 48_000, 4);
            drag.update(point(px((target * 1000.0) as f32), px(50.0)));
            assert_eq!(
                drag.preview_region_with_bpm(snap)
                    .unwrap()
                    .frame_bounds(192_000),
                Some(expected)
            );
            assert_eq!(drag.tempo_resize.unwrap().beat_count, 4);
            assert!(!drag.zero_crossing_applies(snap));
        }
    }

    #[gpui::test]
    fn command_handle_previews_without_committing_until_release(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, _| window.activate_window());
        let baseline = WaveformPlayRegion::new(0.25, 0.5).unwrap();
        browser.update(cx, |view, _| {
            view.waveform = Some(yield_media::WaveformPreview {
                peaks: vec![0.5; 8],
                bands: Vec::new(),
                duration_seconds: 4.0,
                sample_rate: 48_000,
                total_frames: 192_000,
                detail_context: None,
            });
            view.waveform_play_region = Some(baseline);
            view.beat_guide_count = 4;
        });
        cx.refresh().unwrap();
        let bounds = cx.debug_bounds("waveform").unwrap();
        let at = |fraction: f32| {
            point(
                bounds.left() + bounds.size.width * fraction,
                bounds.top() + px(2.0),
            )
        };
        let mut modifier = gpui::Modifiers::none();
        if cfg!(target_os = "macos") {
            modifier.platform = true;
        } else {
            modifier.control = true;
        }
        for abort in [
            Some("escape"),
            Some("blur"),
            Some("pointer loss"),
            Some("viewport"),
            None,
        ] {
            cx.simulate_mouse_down(at(0.5), MouseButton::Left, modifier);
            // Render the newly focused waveform before exercising the keyboard/focus routes.
            cx.refresh().unwrap();
            cx.simulate_mouse_move(at(0.75), MouseButton::Left, gpui::Modifiers::none());
            browser.read_with(cx, |view, _| {
                let drag = view.waveform_region_drag.unwrap();
                assert_eq!(drag.tempo_resize.unwrap().beat_count, 4);
                assert_eq!(
                    drag.preview_region_with_bpm(view.bpm_snap_settings())
                        .unwrap()
                        .frame_bounds(192_000),
                    Some((48_000, 144_000))
                );
                assert_eq!(view.waveform_play_region, Some(baseline));
                assert!(view.playmark_tempo_audition.is_none());
                assert!(view.playback_init_intent.is_none());
            });
            match abort {
                Some("escape") => cx.simulate_keystrokes("escape"),
                Some("blur") => {
                    cx.update(|window, app| {
                        window.focus(&browser.read(app).search_input.read(app).focus_handle());
                    });
                    cx.refresh().unwrap();
                }
                Some("pointer loss") => {
                    browser.update(cx, |view, cx| {
                        assert!(view.update_waveform_region_drag(at(0.75), None, cx));
                    });
                }
                Some("viewport") => {
                    browser.update(cx, |view, cx| {
                        assert!(view.zoom_waveform_to_selection(cx));
                        assert!(view.reset_waveform_viewport(cx));
                    });
                }
                _ => {}
            }
            if let Some(reason) = abort {
                browser.read_with(cx, |view, _| {
                    assert!(
                        view.waveform_region_drag.is_none(),
                        "{reason} should cancel"
                    );
                });
            }
            cx.simulate_mouse_up(at(0.75), MouseButton::Left, gpui::Modifiers::none());
            browser.read_with(cx, |view, _| {
                assert!(view.waveform_region_drag.is_none());
                assert_eq!(
                    view.waveform_play_region.unwrap().frame_bounds(192_000),
                    Some(if abort.is_some() {
                        (48_000, 96_000)
                    } else {
                        (48_000, 144_000)
                    })
                );
            });
        }
    }

    #[test]
    fn beat_guide_projection_returns_bounded_internal_divisions() {
        let region = WaveformPlayRegion::new(0.0, 1.0).unwrap();
        let viewport = WaveformViewport::full();

        let guides =
            beat_guide_projection(region.start, region.end, viewport, 4).collect::<Vec<_>>();
        assert_eq!(guides, vec![0.25, 0.5, 0.75]);
        assert_eq!(
            beat_guide_projection(region.start, region.end, viewport, 1).count(),
            0
        );
        assert_eq!(
            beat_guide_projection(region.start, region.end, viewport, u8::MAX).count(),
            usize::from(MAX_BEAT_COUNT - 1)
        );
    }

    #[test]
    fn beat_guide_projection_clips_to_the_visible_viewport() {
        let region = WaveformPlayRegion::new(0.0, 1.0).unwrap();
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };

        let guides =
            beat_guide_projection(region.start, region.end, viewport, 4).collect::<Vec<_>>();
        assert_eq!(guides, vec![0.0, 0.5, 1.0]);

        let outside_region = WaveformPlayRegion::new(0.0, 0.2).unwrap();
        assert_eq!(
            beat_guide_projection(outside_region.start, outside_region.end, viewport, 4,).count(),
            0
        );
    }

    #[test]
    fn beat_guide_projection_tracks_the_live_resize_region() {
        let bounds = test_bounds(100.0, 100.0);
        let viewport = WaveformViewport::full();
        let baseline = WaveformPlayRegion::new(0.2, 0.6).unwrap();
        let committed =
            beat_guide_projection(baseline.start, baseline.end, viewport, 4).collect::<Vec<_>>();
        let mut drag = WaveformRegionDrag::begin_handle(
            point(px(60.0), px(10.0)),
            0.6,
            bounds,
            viewport,
            baseline,
            WaveformRegionHandle::ResizeEnd,
            13,
        )
        .expect("valid resize handle");
        drag.update(point(px(80.0), px(10.0)));
        let live_region = drag.preview_region().expect("valid live region");
        let live = beat_guide_projection(live_region.start, live_region.end, viewport, 4)
            .collect::<Vec<_>>();

        for (actual, expected) in committed.iter().zip([0.3, 0.4, 0.5]) {
            assert!((actual - expected).abs() < 1.0e-9);
        }
        for (actual, expected) in live.iter().zip([0.35, 0.5, 0.65]) {
            assert!((actual - expected).abs() < 1.0e-6);
        }
        assert_ne!(committed, live);
    }

    #[test]
    fn beat_guides_render_for_both_idle_ranges_but_only_the_active_range_during_drag() {
        let guides = Some((4, gpui::Hsla::default()));
        assert!(beat_guides_for_region(guides, None, WaveformSelectionKind::Play).is_some());
        assert!(beat_guides_for_region(guides, None, WaveformSelectionKind::Edit).is_some());
        assert!(
            beat_guides_for_region(
                guides,
                Some(WaveformSelectionKind::Play),
                WaveformSelectionKind::Play
            )
            .is_some()
        );
        assert!(
            beat_guides_for_region(
                guides,
                Some(WaveformSelectionKind::Play),
                WaveformSelectionKind::Edit
            )
            .is_none()
        );
        assert!(
            beat_guides_for_region(
                guides,
                Some(WaveformSelectionKind::Edit),
                WaveformSelectionKind::Play
            )
            .is_none()
        );
        assert!(
            beat_guides_for_region(
                guides,
                Some(WaveformSelectionKind::Edit),
                WaveformSelectionKind::Edit
            )
            .is_some()
        );
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
    fn waveform_region_keyboard_routing_keeps_coarse_and_fine_modifiers_distinct() {
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
            Some(WaveformRegionSlide::FineRight)
        );
        assert_eq!(
            waveform_region_navigation_key("right", false, false, true, false),
            Some(WaveformRegionSlide::FineRight)
        );
        assert_eq!(
            waveform_region_navigation_key("arrowleft", false, true, false, false),
            Some(WaveformRegionSlide::FineLeft)
        );
        assert_eq!(
            waveform_region_navigation_key("left", false, false, true, false),
            Some(WaveformRegionSlide::FineLeft)
        );
        assert_eq!(
            waveform_region_navigation_key("right", false, true, true, false),
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
    fn playmark_slide_takes_only_full_width_steps_in_playback_frames() {
        let total = 103;
        let mut region = WaveformPlayRegion::new(5.0 / 103.0, 14.0 / 103.0).unwrap();
        assert!(
            region
                .shifted_by_frames(WaveformRegionSlide::Left, total)
                .is_none()
        );
        for start in (1..=9).map(|step| 5 + 9 * step) {
            region = region
                .shifted_by_frames(WaveformRegionSlide::Right, total)
                .expect("one full playmark-width slide");
            assert_eq!(playback_frames(region, total), (start, start + 9));
        }
        assert!(
            region
                .shifted_by_frames(WaveformRegionSlide::Right, total)
                .is_none()
        );
        for start in (0..9).rev().map(|step| 5 + 9 * step) {
            region = region
                .shifted_by_frames(WaveformRegionSlide::Left, total)
                .expect("one full playmark-width slide");
            assert_eq!(playback_frames(region, total), (start, start + 9));
        }
    }

    #[test]
    fn fine_region_nudges_preserve_exact_frames_and_clamp_without_repeat_drift() {
        let mut region = WaveformPlayRegion::new(0.2, 0.4).expect("play region");
        for start in 201..=800 {
            region = region
                .shifted_by_frames(WaveformRegionSlide::FineRight, 1_000)
                .expect("one-frame nudge");
            assert_eq!(playback_frames(region, 1_000), (start, start + 200));
        }
        let right_edge = region
            .shifted_by_frames(WaveformRegionSlide::FineRight, 1_000)
            .expect("clamped nudge");
        assert_eq!(right_edge, region);
        for start in (0..800).rev() {
            region = region
                .shifted_by_frames(WaveformRegionSlide::FineLeft, 1_000)
                .expect("one-frame nudge");
            assert_eq!(playback_frames(region, 1_000), (start, start + 200));
        }
        assert_eq!(
            region.shifted_by_frames(WaveformRegionSlide::FineLeft, 1_000),
            Some(region)
        );
        assert_eq!(
            region.shifted_by_frames(WaveformRegionSlide::FineRight, 0),
            None
        );

        for total_frames in [44_100, 1_000_003, 50_000_000] {
            let start = total_frames / 3;
            let width = total_frames / 5;
            let region = WaveformPlayRegion::new(
                start as f64 / total_frames as f64,
                (start + width) as f64 / total_frames as f64,
            )
            .expect("frame-aligned region");
            let before = playback_frames(region, total_frames);
            let nudged = region
                .shifted_by_frames(WaveformRegionSlide::FineRight, total_frames)
                .expect("one-frame nudge");
            let after = playback_frames(nudged, total_frames);
            assert_eq!(after.0, before.0 + 1);
            assert_eq!(after.1 - after.0, before.1 - before.0);
        }
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
    #[ignore = "manual bounded cache-growth measurement (OPT-1736)"]
    fn bounded_waveform_cache_workload() {
        use std::{hint::black_box, time::Instant};

        let started = Instant::now();
        let cache = WaveformRenderCache::default();
        let bands = vec![[0.5, 0.4, 0.3, 0.6]; 65_536];
        let bounds = test_bounds(1_120.0, 180.0);
        let selection = WaveformEditRegion::new(0.25, 0.75).unwrap();
        for generation in 0..32 {
            let iteration = Instant::now();
            for _ in 0..16 {
                black_box(cache.paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    WaveformViewport::full(),
                    (0.0, 1.0),
                    generation,
                ));
                black_box(cache.gain_preview_paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    WaveformViewport::full(),
                    (0.0, 1.0),
                    generation,
                    selection,
                    1.5,
                ));
            }
            let state = cache.state.borrow();
            let Some(WaveformGainPreviewGeometry::Bands { paths, .. }) =
                state.gain_preview_geometry.as_ref()
            else {
                panic!("expected cached preview");
            };
            assert_eq!(state.paths.len(), 4);
            assert_eq!(paths.len(), 4);
            assert_eq!(Rc::strong_count(&state.paths), 1);
            assert_eq!(Rc::strong_count(paths), 1);
            assert_eq!(state.rebuild_count, generation as usize + 1);
            assert_eq!(state.gain_preview_rebuild_count, generation as usize + 1);
            eprintln!(
                "YIELD_WORKLOAD={{\"kind\":\"waveform-cache\",\"generation\":{generation},\"inputs\":65536,\"width_px\":1120,\"hits_per_generation\":15,\"base_slots\":1,\"preview_slots\":1,\"base_paths\":4,\"preview_paths\":4,\"external_path_refs\":0,\"iteration_ns\":{}}}",
                iteration.elapsed().as_nanos()
            );
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }

    #[test]
    #[ignore = "manual waveform geometry cache benchmark"]
    fn waveform_geometry_cache_benchmark() {
        use std::{hint::black_box, time::Instant};

        fn report(
            kind: &str,
            inputs: usize,
            width: usize,
            misses: usize,
            cold: std::time::Duration,
            warm: std::time::Duration,
        ) {
            eprintln!(
                "YIELD_BASELINE={{\"schema\":\"yield.waveform-geometry.v1\",\"kind\":\"{kind}\",\"input_columns\":{inputs},\"width_px\":{width},\"height_px\":180,\"miss_iterations\":{misses},\"hit_iterations\":100,\"miss_total_ns\":{},\"hit_total_ns\":{}}}",
                cold.as_nanos(),
                warm.as_nanos()
            );
        }

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
        report("geometry", 2_048, 640, 100, cold, warm);

        let selection = WaveformEditRegion::new(0.25, 0.75).expect("benchmark selection");
        for (preview_len, generation) in [(2_048, 200), (65_536, 300)] {
            let bands = vec![[0.5, 0.4, 0.3, 0.6]; preview_len];
            let bounds = test_bounds(1_120.0, 180.0);
            let viewport = WaveformViewport::full();
            let cache = WaveformRenderCache::default();
            black_box(cache.paths_for(generation, &bands, 1.0, bounds, viewport, (0.0, 1.0), 0));

            let cold_start = Instant::now();
            for step in 0..8 {
                let gain = step as f32 * 0.5;
                black_box(cache.gain_preview_paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    gain,
                ));
            }
            let cold = cold_start.elapsed();

            let warm_start = Instant::now();
            for _ in 0..100 {
                black_box(cache.gain_preview_paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    3.5,
                ));
            }
            let warm = warm_start.elapsed();
            eprintln!(
                "gain preview {preview_len} inputs at 1120px: 8 distinct gains {cold:?}; 100 cache hits {warm:?}"
            );
            report("gain", preview_len, 1_120, 8, cold, warm);
            let settings_for = |step| {
                WaveformEditSettings::new(
                    1.0,
                    Some(WaveformEditFade::new(step as f32 / 16.0, 0.5, 0.25, 0.5).unwrap()),
                    Some(WaveformEditFade::new(0.25, 0.5, 0.25, 0.5).unwrap()),
                )
                .unwrap()
            };
            let cold_start = Instant::now();
            for step in 0..8 {
                black_box(cache.edit_preview_paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    settings_for(step),
                ));
            }
            let cold = cold_start.elapsed();
            let warm_start = Instant::now();
            for _ in 0..100 {
                black_box(cache.edit_preview_paths_for(
                    generation,
                    &bands,
                    1.0,
                    bounds,
                    viewport,
                    (0.0, 1.0),
                    0,
                    selection,
                    settings_for(7),
                ));
            }
            let warm = warm_start.elapsed();
            eprintln!(
                "fade preview {preview_len} inputs at 1120px: 8 distinct fades {cold:?}; 100 cache hits {warm:?}"
            );
            report("fade", preview_len, 1_120, 8, cold, warm);
        }
    }
}
