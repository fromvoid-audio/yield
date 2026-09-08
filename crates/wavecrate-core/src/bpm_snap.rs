#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral whole-BPM snapping for normalized waveform selection edges.

/// Snap a moving selection edge so the configured beat count spans a whole BPM value.
///
/// `fixed_ratio` is the non-dragged selection edge, and `moving_ratio` is the current candidate
/// for the dragged edge. Both ratios are normalized file coordinates, while
/// `audio_duration_seconds` is the full file duration. The returned edge preserves the current
/// drag direction around the fixed edge and uses the nearest positive whole BPM. When
/// `constrain_to_audio` is `true`, the selected BPM is raised as needed so the snapped edge fits
/// between the fixed edge and the corresponding audio boundary. When it is `false`, the native
/// calculation is allowed to return a ratio outside `0.0..=1.0`.
///
/// Non-finite ratios or duration, a non-positive duration, a zero beat count, and a zero or
/// invalid derived span leave `moving_ratio` unchanged. The arithmetic intentionally remains in
/// the native helper's `f32` order so preview and committed selection calculations agree.
pub fn snap_resize_ratio_to_whole_bpm(
    fixed_ratio: f32,
    moving_ratio: f32,
    audio_duration_seconds: f32,
    beat_count: u8,
    constrain_to_audio: bool,
) -> f32 {
    let delta = moving_ratio - fixed_ratio;
    let selection_duration_seconds = delta.abs() * audio_duration_seconds;
    if !fixed_ratio.is_finite()
        || !moving_ratio.is_finite()
        || !audio_duration_seconds.is_finite()
        || audio_duration_seconds <= 0.0
        || beat_count == 0
        || !selection_duration_seconds.is_finite()
        || selection_duration_seconds <= 0.0
    {
        return moving_ratio;
    }

    let bpm = f32::from(beat_count) * 60.0 / selection_duration_seconds;
    if !bpm.is_finite() || bpm <= 0.0 {
        return moving_ratio;
    }
    let mut whole_bpm = bpm.round().max(1.0);
    if constrain_to_audio {
        let available_width = if delta.is_sign_positive() {
            1.0 - fixed_ratio
        } else {
            fixed_ratio
        };
        if available_width.is_finite() && available_width > 0.0 {
            let minimum_bpm =
                f32::from(beat_count) * 60.0 / (available_width * audio_duration_seconds);
            if minimum_bpm.is_finite() {
                whole_bpm = whole_bpm.max(minimum_bpm.ceil());
            }
        }
    }
    let snapped_duration_seconds = f32::from(beat_count) * 60.0 / whole_bpm;
    let snapped_width = snapped_duration_seconds / audio_duration_seconds;
    if !snapped_width.is_finite() || snapped_width <= 0.0 {
        return moving_ratio;
    }

    fixed_ratio + delta.signum() * snapped_width
}

#[cfg(test)]
mod tests {
    use super::snap_resize_ratio_to_whole_bpm;

    #[test]
    fn resize_ratio_snaps_beat_span_to_nearest_whole_bpm() {
        let snapped = snap_resize_ratio_to_whole_bpm(0.2, 0.61, 10.0, 4, true);
        let duration = (snapped - 0.2) * 10.0;
        let bpm = 4.0 * 60.0 / duration;

        assert!((bpm - 59.0).abs() < 0.001);
    }

    #[test]
    fn resize_ratio_preserves_drag_direction_around_fixed_edge() {
        let snapped = snap_resize_ratio_to_whole_bpm(0.8, 0.39, 10.0, 4, true);

        assert!(snapped < 0.8);
        assert!((4.0 * 60.0 / ((0.8 - snapped) * 10.0) - 59.0).abs() < 0.001);
    }

    #[test]
    fn invalid_inputs_leave_resize_ratio_unchanged() {
        assert_eq!(
            snap_resize_ratio_to_whole_bpm(0.2, 0.61, 0.0, 4, true),
            0.61
        );
        assert_eq!(
            snap_resize_ratio_to_whole_bpm(0.2, 0.61, 10.0, 0, true),
            0.61
        );
    }

    #[test]
    fn constrained_snap_chooses_a_whole_bpm_that_fits_at_audio_edge() {
        let snapped = snap_resize_ratio_to_whole_bpm(0.2, 0.99, 10.0, 4, true);
        let bpm = 4.0 * 60.0 / ((snapped - 0.2) * 10.0);

        assert!(snapped <= 1.0);
        assert!((bpm - bpm.round()).abs() < 0.001);
    }

    #[test]
    fn crossing_fixed_edge_keeps_the_current_drag_direction() {
        let toward_end = snap_resize_ratio_to_whole_bpm(0.35, 0.9, 10.0, 4, true);
        let toward_start = snap_resize_ratio_to_whole_bpm(0.65, 0.1, 10.0, 4, true);

        assert!(toward_end > 0.35);
        assert!(toward_start < 0.65);
    }

    #[test]
    fn unconstrained_snap_can_extend_past_an_audio_boundary() {
        let unconstrained = snap_resize_ratio_to_whole_bpm(0.9, 1.0, 78.94737, 4, false);
        let constrained = snap_resize_ratio_to_whole_bpm(0.9, 1.0, 78.94737, 4, true);

        assert!(unconstrained > 1.0);
        assert!(constrained <= 1.0);
    }

    #[test]
    fn nonfinite_and_zero_width_inputs_are_no_ops() {
        let moving = 0.61;
        assert_eq!(
            snap_resize_ratio_to_whole_bpm(f32::NAN, moving, 10.0, 4, true),
            moving
        );
        assert_eq!(
            snap_resize_ratio_to_whole_bpm(0.2, moving, f32::INFINITY, 4, true),
            moving
        );
        assert!(
            snap_resize_ratio_to_whole_bpm(0.2, f32::NAN, 10.0, 4, true).is_nan(),
            "a non-finite moving ratio is returned unchanged"
        );
        assert_eq!(snap_resize_ratio_to_whole_bpm(0.2, 0.2, 10.0, 4, true), 0.2);
    }

    #[test]
    fn tiny_span_and_near_audio_boundary_remain_finite_and_bounded() {
        let tiny = snap_resize_ratio_to_whole_bpm(0.2, 0.2 + f32::EPSILON, 10.0, 4, true);
        let near_boundary = snap_resize_ratio_to_whole_bpm(0.999, 1.0, 10.0, 4, true);

        assert!(tiny.is_finite());
        assert!(tiny > 0.2);
        assert!(near_boundary.is_finite());
        assert!((0.0..=1.0).contains(&near_boundary));
    }
}
