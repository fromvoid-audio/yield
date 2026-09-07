#![deny(missing_docs)]
#![deny(warnings)]

//! Browser motion presentation and frame-cadence helpers.

use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    Bounds, Hsla, Pixels, UniformListScrollHandle, Window, canvas, fill, linear_color_stop,
    linear_gradient, point, prelude::*, px,
};
use wavecrate_design::motion::{FocusEnvelope, OpacityTransition};
use wavecrate_design::{Color, Motion};

use crate::theme::color;

/// Width of the immediate pointer-hover rail used by browser rows.
pub(super) const HOVER_RAIL_WIDTH: f32 = 2.0;

/// Alpha used by the pale pointer-hover rail.
pub(super) const HOVER_RAIL_ALPHA: u8 = 180;

/// Height of the source-processing track at the bottom of a source row.
pub(super) const SOURCE_SWEEP_TRACK_HEIGHT: f32 = 1.5;

/// Fraction of the source-processing track covered by its moving segment.
pub(super) const SOURCE_SWEEP_SEGMENT_FRACTION: f32 = 0.28;

/// Minimum width of the source-processing segment in logical pixels.
pub(super) const SOURCE_SWEEP_MIN_SEGMENT_WIDTH: f32 = 28.0;

/// Alpha of the quiet source-processing track.
pub(super) const SOURCE_SWEEP_TRACK_ALPHA: u8 = 42;

/// Alpha of the moving source-processing segment.
pub(super) const SOURCE_SWEEP_SEGMENT_ALPHA: u8 = 235;

/// Width of a leading focus marker in logical pixels.
pub(super) const FOCUS_MARKER_WIDTH: f32 = 6.0;

/// Height of a vertical scroll-edge affordance in logical pixels.
pub(super) const SCROLL_EDGE_FADE_HEIGHT: f32 = 48.0;

const SCROLL_EDGE_EPSILON: f32 = 0.5;

/// Which browser surface owns the current focus cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FocusSurface {
    /// No focused browser row is currently retained.
    None,
    /// The sample list owns the current focus cue.
    Sample,
    /// The source list owns the current focus cue.
    Source,
    /// The folder tree owns the current focus cue.
    Folder,
}

/// The input path that created a browser focus cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FocusTrigger {
    /// Pointer selection fades immediately.
    Pointer,
    /// Keyboard selection holds before fading.
    Keyboard,
}

impl FocusTrigger {
    pub(super) fn envelope(self, motion: Motion) -> FocusEnvelope {
        match self {
            Self::Pointer => motion.pointer_focus_envelope(),
            Self::Keyboard => motion.focus_envelope(),
        }
    }
}

/// Stable path identity for the currently focused browser row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FocusTarget {
    /// A sample path in the main browser list.
    Sample(std::path::PathBuf),
    /// A source root path in the sidebar source list.
    Source(std::path::PathBuf),
    /// A folder path in the sidebar folder tree.
    Folder(std::path::PathBuf),
}

impl FocusTarget {
    /// Return the surface represented by this stable identity.
    pub(super) const fn surface(&self) -> FocusSurface {
        match self {
            Self::Sample(_) => FocusSurface::Sample,
            Self::Source(_) => FocusSurface::Source,
            Self::Folder(_) => FocusSurface::Folder,
        }
    }

    /// Return whether this target is the given sample path.
    pub(super) fn matches_sample(&self, path: &Path) -> bool {
        matches!(self, Self::Sample(target) if target == path)
    }

    /// Return whether this target is the given source root path.
    pub(super) fn matches_source(&self, path: &Path) -> bool {
        matches!(self, Self::Source(target) if target == path)
    }

    /// Return whether this target is the given folder path.
    pub(super) fn matches_folder(&self, path: &Path) -> bool {
        matches!(self, Self::Folder(target) if target == path)
    }
}

/// Directional scroll-edge visibility derived from a real scroll handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ScrollEdges {
    /// Whether content is clipped above the viewport.
    pub(super) top: bool,
    /// Whether content is clipped below the viewport.
    pub(super) bottom: bool,
}

impl ScrollEdges {
    /// Derive directional overflow from the viewport bounds, offset, and maximum offset.
    pub(super) fn from_scroll(
        bounds: Bounds<Pixels>,
        offset: gpui::Point<Pixels>,
        max_offset: gpui::Size<Pixels>,
    ) -> Self {
        let viewport_height = f32::from(bounds.size.height);
        let offset_y = f32::from(offset.y);
        let maximum = f32::from(max_offset.height);
        if !viewport_height.is_finite()
            || viewport_height <= 0.0
            || !offset_y.is_finite()
            || !maximum.is_finite()
            || maximum <= SCROLL_EDGE_EPSILON
        {
            return Self::default();
        }
        let current = (-offset_y).clamp(0.0, maximum);
        Self {
            top: current > SCROLL_EDGE_EPSILON,
            bottom: current < maximum - SCROLL_EDGE_EPSILON,
        }
    }
}

/// Sampled opacity values for the two vertical scroll edges.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ScrollEdgeFrame {
    /// Opacity of the top edge cue.
    pub(super) top: f32,
    /// Opacity of the bottom edge cue.
    pub(super) bottom: f32,
}

/// Retargetable opacity state for one virtual list's two scroll edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ScrollEdgeTransitions {
    top: OpacityTransition,
    bottom: OpacityTransition,
    motion: Motion,
}

impl ScrollEdgeTransitions {
    /// Create settled edge transitions using the supplied motion settings.
    pub(super) fn new(motion: Motion) -> Self {
        Self {
            top: motion.overflow_transition(0.0),
            bottom: motion.overflow_transition(0.0),
            motion,
        }
    }

    /// Reconfigure timing while retaining the currently sampled opacity.
    pub(super) fn update_motion(&mut self, motion: Motion, now: Duration) {
        if self.motion == motion {
            return;
        }
        let top = self.top.value_at(now);
        let bottom = self.bottom.value_at(now);
        self.top = OpacityTransition::new(top, motion.overflow_fade(), motion.enabled);
        self.bottom = OpacityTransition::new(bottom, motion.overflow_fade(), motion.enabled);
        self.motion = motion;
    }

    /// Retarget both edges while preserving their current visual values.
    pub(super) fn retarget(&mut self, edges: ScrollEdges, now: Duration) {
        self.top.retarget(f32::from(edges.top), now);
        self.bottom.retarget(f32::from(edges.bottom), now);
    }

    /// Sample both edge opacities.
    pub(super) fn frame(&self, now: Duration) -> ScrollEdgeFrame {
        ScrollEdgeFrame {
            top: self.top.value_at(now),
            bottom: self.bottom.value_at(now),
        }
    }

    /// Return whether either edge still needs another animation sample.
    pub(super) fn needs_animation_at(&self, now: Duration) -> bool {
        self.top.needs_animation_at(now) || self.bottom.needs_animation_at(now)
    }
}

/// A sampled browser motion frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MotionFrame {
    /// Alpha sampled for the worker status pulse.
    pub(super) worker_alpha: u8,
    /// Phase sampled for the source-processing sweep.
    pub(super) source_phase: f32,
    /// Alpha sampled for the current stable browser focus cue.
    pub(super) focus_alpha: u8,
    /// Surface owning the current stable browser focus cue.
    pub(super) focus_surface: FocusSurface,
}

impl MotionFrame {
    /// Sample all browser motion values from one elapsed-time value.
    pub(super) fn sample(motion: Motion, elapsed: Duration) -> Self {
        Self {
            worker_alpha: motion.worker_pulse_alpha_at(elapsed),
            source_phase: motion.source_sweep_phase_at(elapsed),
            focus_alpha: 0,
            focus_surface: FocusSurface::None,
        }
    }
}

/// The kinds of work that may justify an animation frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingWork {
    /// A source scan is still current and running.
    pub(super) scan: bool,
    /// The selected waveform request has not reached a terminal result.
    pub(super) waveform: bool,
    /// The current user audio-runtime initialization intent is still pending.
    pub(super) audio_initialization: bool,
}

impl PendingWork {
    /// Construct a pending-work snapshot.
    pub(super) const fn new(scan: bool, waveform: bool, audio_initialization: bool) -> Self {
        Self {
            scan,
            waveform,
            audio_initialization,
        }
    }

    /// Return whether any current work is pending.
    pub(super) const fn any(self) -> bool {
        self.scan || self.waveform || self.audio_initialization
    }
}

/// Return whether the browser should request another animation frame.
pub(super) const fn should_schedule_frames(
    enabled: bool,
    pending: PendingWork,
    transition_pending: bool,
) -> bool {
    enabled && (pending.any() || transition_pending)
}

/// Derive pending work from current lifecycle signals rather than retained task handles.
pub(super) const fn pending_work(
    scan_pending: bool,
    waveform_pending: bool,
    audio_initialization_task_exists: bool,
    audio_initialization_intent_exists: bool,
) -> PendingWork {
    PendingWork::new(
        scan_pending,
        waveform_pending,
        audio_initialization_task_exists && audio_initialization_intent_exists,
    )
}

/// Paint a source row's retained bottom track and moving segment.
pub(super) fn source_sweep_overlay(
    phase: f32,
    track_color: Hsla,
    segment_color: Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            if !width.is_finite() || width <= 0.0 {
                return;
            }
            window.paint_quad(fill(bounds, track_color));
            for (start, end) in source_sweep_segment_bounds(width, phase) {
                if end <= start {
                    continue;
                }
                let segment_width = (width * SOURCE_SWEEP_SEGMENT_FRACTION)
                    .clamp(SOURCE_SWEEP_MIN_SEGMENT_WIDTH.min(width), width);
                let head = width
                    * if phase.is_finite() {
                        phase.rem_euclid(1.0)
                    } else {
                        0.0
                    };
                let tail = head - segment_width;
                let wrap_offset = if tail < 0.0 && end >= width {
                    -width
                } else {
                    0.0
                };
                paint_sweep_span(
                    window,
                    bounds,
                    start,
                    end,
                    tail,
                    segment_width,
                    wrap_offset,
                    segment_color,
                );
            }
        },
    )
    .absolute()
    .left_0()
    .right_0()
    .bottom_0()
    .h(px(SOURCE_SWEEP_TRACK_HEIGHT))
}

fn paint_sweep_span(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    start: f32,
    end: f32,
    tail: f32,
    segment_width: f32,
    wrap_offset: f32,
    segment_color: Hsla,
) {
    let segment = Bounds::from_corners(
        point(bounds.left() + px(start), bounds.top()),
        point(bounds.left() + px(end), bounds.bottom()),
    );
    let start_alpha = ((start + wrap_offset - tail) / segment_width).clamp(0.0, 1.0);
    let end_alpha = ((end + wrap_offset - tail) / segment_width).clamp(0.0, 1.0);
    let gradient = linear_gradient(
        90.0,
        linear_color_stop(segment_color.opacity(start_alpha), 0.0),
        linear_color_stop(segment_color.opacity(end_alpha), 1.0),
    );
    window.paint_quad(fill(segment, gradient));
}

/// Return up to two non-wrapping segment spans for a normalized sweep phase.
pub(super) fn source_sweep_segment_bounds(width: f32, phase: f32) -> [(f32, f32); 2] {
    if !width.is_finite() || width <= 0.0 {
        return [(0.0, 0.0); 2];
    }
    let segment_width = (width * SOURCE_SWEEP_SEGMENT_FRACTION)
        .clamp(SOURCE_SWEEP_MIN_SEGMENT_WIDTH.min(width), width);
    let phase = if phase.is_finite() {
        phase.rem_euclid(1.0)
    } else {
        0.0
    };
    let head = width * phase;
    let tail = head - segment_width;
    if tail < 0.0 {
        [(0.0, head), (width + tail, width)]
    } else {
        [(tail, head), (0.0, 0.0)]
    }
}

/// Convert a theme color to an explicit alpha variant for a motion primitive.
pub(super) fn with_alpha(value: Color, alpha: u8) -> Hsla {
    color(Color::rgba(value.red(), value.green(), value.blue(), alpha))
}

/// Return a slightly deeper surface tone with the requested opacity.
pub(super) fn edge_fade_tone(value: Color, opacity: f32) -> Hsla {
    color(Color::rgba(
        value.red().saturating_sub(8),
        value.green().saturating_sub(8),
        value.blue().saturating_sub(8),
        value.alpha(),
    ))
    .opacity(opacity.clamp(0.0, 1.0))
}

/// Read directional overflow from a GPUI uniform-list scroll handle.
pub(super) fn scroll_edges(handle: &UniformListScrollHandle) -> ScrollEdges {
    let base_handle = handle.0.borrow().base_handle.clone();
    ScrollEdges::from_scroll(
        base_handle.bounds(),
        base_handle.offset(),
        base_handle.max_offset(),
    )
}

/// Paint noninteractive vertical edge gradients over a clipped virtual list.
pub(super) fn scroll_edge_fades(
    handle: UniformListScrollHandle,
    transitions: Rc<RefCell<ScrollEdgeTransitions>>,
    motion: Motion,
    clock_origin: Instant,
    background: Color,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let now = Instant::now().duration_since(clock_origin);
            let handle_bounds = handle.0.borrow().base_handle.bounds();
            let paint_bounds = bounds.intersect(&handle_bounds);
            let edges = scroll_edges(&handle);
            let mut transitions = transitions.borrow_mut();
            transitions.update_motion(motion, now);
            transitions.retarget(edges, now);
            let frame = transitions.frame(now);
            let width = f32::from(paint_bounds.size.width);
            let height = f32::from(paint_bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            if motion.enabled && transitions.needs_animation_at(now) {
                window.request_animation_frame();
            }
            let fade_height = SCROLL_EDGE_FADE_HEIGHT.min(height);
            if frame.top > 0.0 {
                let fade_bounds = Bounds::from_corners(
                    point(paint_bounds.left(), paint_bounds.top()),
                    point(paint_bounds.right(), paint_bounds.top() + px(fade_height)),
                );
                let tone = edge_fade_tone(background, frame.top);
                let gradient = linear_gradient(
                    180.0,
                    linear_color_stop(tone, 0.0),
                    linear_color_stop(tone.opacity(0.0), 1.0),
                );
                window.paint_quad(fill(fade_bounds, gradient));
            }
            if frame.bottom > 0.0 {
                let fade_bounds = Bounds::from_corners(
                    point(paint_bounds.left(), paint_bounds.bottom() - px(fade_height)),
                    paint_bounds.bottom_right(),
                );
                let tone = edge_fade_tone(background, frame.bottom);
                let gradient = linear_gradient(
                    180.0,
                    linear_color_stop(tone.opacity(0.0), 0.0),
                    linear_color_stop(tone, 1.0),
                );
                window.paint_quad(fill(fade_bounds, gradient));
            }
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

    #[test]
    fn frames_only_schedule_for_current_pending_work() {
        assert!(should_schedule_frames(
            true,
            PendingWork::new(true, false, false),
            false,
        ));
        assert!(should_schedule_frames(
            true,
            PendingWork::new(false, true, false),
            false,
        ));
        assert!(should_schedule_frames(
            true,
            PendingWork::new(false, false, true),
            false,
        ));
        assert!(should_schedule_frames(true, PendingWork::default(), true));
        assert!(!should_schedule_frames(true, PendingWork::default(), false));
    }

    #[test]
    fn disabled_motion_keeps_busy_work_static() {
        let pending = PendingWork::new(true, true, true);

        assert!(!should_schedule_frames(false, pending, true));
        let frame = MotionFrame::sample(
            Motion {
                enabled: false,
                ..Motion::default()
            },
            Duration::from_secs(9),
        );
        assert_eq!(frame.worker_alpha, u8::MAX);
        assert_eq!(frame.source_phase, 0.0);
    }

    #[test]
    fn terminal_work_stops_the_cadence() {
        let pending = PendingWork::new(false, false, false);

        assert!(!pending.any());
        assert!(!should_schedule_frames(true, pending, false));
    }

    #[test]
    fn retained_audio_task_without_current_intent_is_terminal() {
        let pending = pending_work(false, false, true, false);

        assert_eq!(pending, PendingWork::default());
        assert!(!should_schedule_frames(true, pending, false));
    }

    #[test]
    fn pending_snapshot_preserves_current_scan_waveform_and_audio_work() {
        assert_eq!(
            pending_work(true, false, false, false),
            PendingWork::new(true, false, false)
        );
        assert_eq!(
            pending_work(false, true, false, false),
            PendingWork::new(false, true, false)
        );
        assert_eq!(
            pending_work(false, false, true, true),
            PendingWork::new(false, false, true)
        );
    }

    #[test]
    fn source_sweep_honors_fraction_and_minimum_width() {
        let spans = source_sweep_segment_bounds(100.0, 0.5);
        assert_eq!(spans, [(22.0, 50.0), (0.0, 0.0)]);

        let spans = source_sweep_segment_bounds(20.0, 0.5);
        assert_eq!(spans, [(0.0, 10.0), (10.0, 20.0)]);

        let spans = source_sweep_segment_bounds(20.0, 0.0);
        assert_eq!(spans, [(0.0, 0.0), (0.0, 20.0)]);
    }

    #[test]
    fn focus_target_follows_stable_path_identity() {
        let target = FocusTarget::Sample("samples/kick.wav".into());

        assert!(target.matches_sample(Path::new("samples/kick.wav")));
        assert!(!target.matches_sample(Path::new("samples/snare.wav")));
        assert!(!target.matches_source(Path::new("samples/kick.wav")));
    }

    #[test]
    fn scroll_edges_use_actual_offset_and_terminal_transition_cadence() {
        let bounds = Bounds::from_corners(point(px(0.0), px(0.0)), point(px(120.0), px(100.0)));
        let edges = ScrollEdges::from_scroll(
            bounds,
            point(px(0.0), px(-25.0)),
            gpui::size(px(0.0), px(100.0)),
        );
        assert_eq!(
            edges,
            ScrollEdges {
                top: true,
                bottom: true
            }
        );

        let motion = Motion::default();
        let mut transitions = ScrollEdgeTransitions::new(motion);
        transitions.retarget(edges, Duration::ZERO);
        assert!(transitions.needs_animation_at(Duration::from_millis(1)));
        assert_eq!(transitions.frame(Duration::from_millis(260)).top, 1.0);
        assert_eq!(transitions.frame(Duration::from_millis(260)).bottom, 1.0);
        assert!(!transitions.needs_animation_at(Duration::from_millis(260)));

        transitions.retarget(ScrollEdges::default(), Duration::from_millis(260));
        let before_retarget = transitions.frame(Duration::from_millis(390));
        transitions.retarget(
            ScrollEdges {
                top: true,
                bottom: false,
            },
            Duration::from_millis(390),
        );
        assert!(
            (transitions.frame(Duration::from_millis(390)).top - before_retarget.top).abs()
                < 0.0001
        );
    }
}
