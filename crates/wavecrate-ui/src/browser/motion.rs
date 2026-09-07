#![deny(missing_docs)]
#![deny(warnings)]

//! Browser motion presentation and frame-cadence helpers.

use std::time::Duration;

use gpui::{
    Bounds, Hsla, Pixels, Window, canvas, fill, linear_color_stop, linear_gradient, point,
    prelude::*, px,
};
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

/// A sampled browser motion frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MotionFrame {
    /// Alpha sampled for the worker status pulse.
    pub(super) worker_alpha: u8,
    /// Phase sampled for the source-processing sweep.
    pub(super) source_phase: f32,
}

impl MotionFrame {
    /// Sample all browser motion values from one elapsed-time value.
    pub(super) fn sample(motion: Motion, elapsed: Duration) -> Self {
        Self {
            worker_alpha: motion.worker_pulse_alpha_at(elapsed),
            source_phase: motion.source_sweep_phase_at(elapsed),
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
pub(super) const fn should_schedule_frames(enabled: bool, pending: PendingWork) -> bool {
    enabled && pending.any()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_only_schedule_for_current_pending_work() {
        assert!(should_schedule_frames(
            true,
            PendingWork::new(true, false, false)
        ));
        assert!(should_schedule_frames(
            true,
            PendingWork::new(false, true, false)
        ));
        assert!(should_schedule_frames(
            true,
            PendingWork::new(false, false, true)
        ));
        assert!(!should_schedule_frames(true, PendingWork::default()));
    }

    #[test]
    fn disabled_motion_keeps_busy_work_static() {
        let pending = PendingWork::new(true, true, true);

        assert!(!should_schedule_frames(false, pending));
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
        assert!(!should_schedule_frames(true, pending));
    }

    #[test]
    fn retained_audio_task_without_current_intent_is_terminal() {
        let pending = pending_work(false, false, true, false);

        assert_eq!(pending, PendingWork::default());
        assert!(!should_schedule_frames(true, pending));
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
}
