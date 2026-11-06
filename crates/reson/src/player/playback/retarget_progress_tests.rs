use super::*;
use crate::player::{
    PlaybackChannelLayout, PlaybackSeekBehavior, PlaybackSourceIdentity, PlaybackSourceKind,
    PlaybackSpanRequest,
};

fn span_plan(offset_frame: u64) -> PlaybackSpanPlan {
    span_plan_with(1.0, 0.2, 0.8, offset_frame, 1_000)
}

fn span_plan_with(
    duration: f32,
    start: f32,
    end: f32,
    offset_frame: u64,
    sample_rate: u32,
) -> PlaybackSpanPlan {
    PlaybackSpanPlan::new(
        PlaybackSourceIdentity::new(PlaybackSourceKind::Bytes, None),
        PlaybackChannelLayout::new(1, sample_rate).expect("layout"),
        PlaybackSpanRequest::new(
            start,
            end,
            duration,
            true,
            PlaybackSeekBehavior::FrameOffset(offset_frame),
        ),
    )
    .expect("span plan")
}

#[test]
fn non_seeking_retarget_rebases_progress_from_runtime_time() {
    let plan = span_plan(40);

    let offset = live_retarget_progress_offset_frames(&plan, false, Some(525));

    assert_eq!(offset, 325);
}

#[test]
fn non_seeking_retarget_wraps_progress_that_left_the_new_span() {
    let plan = span_plan(40);

    let offset = live_retarget_progress_offset_frames(&plan, false, Some(825));

    assert_eq!(offset, 0);
}

#[test]
fn seeking_retarget_uses_the_requested_offset() {
    let plan = span_plan(40);

    let offset = live_retarget_progress_offset_frames(&plan, true, Some(525));

    assert_eq!(offset, 40);
}

#[test]
fn guarded_retarget_preserves_exact_frame_inside_native_guard() {
    let plan = span_plan(40);
    let inside_frame = plan.end_frame().saturating_sub(11);

    assert_eq!(
        resolve_retarget_seek(&plan, false, true, Some(inside_frame)),
        RetargetSeekDecision::Preserve {
            offset_frames: inside_frame - plan.start_frame()
        }
    );
    assert_eq!(
        resolve_retarget_seek(&plan, false, true, Some(plan.end_frame() - 10)),
        RetargetSeekDecision::RestartAtStart
    );
}

#[test]
fn guarded_retarget_restarts_for_missing_or_outside_progress() {
    let plan = span_plan(40);

    for current_frame in [
        None,
        Some(plan.start_frame().saturating_sub(1)),
        Some(plan.end_frame()),
    ] {
        assert_eq!(
            resolve_retarget_seek(&plan, false, true, current_frame),
            RetargetSeekDecision::RestartAtStart
        );
    }
}

#[test]
fn explicit_seek_takes_precedence_over_guarded_progress() {
    let plan = span_plan(40);

    assert_eq!(
        resolve_retarget_seek(&plan, true, true, Some(plan.start_frame())),
        RetargetSeekDecision::Requested { offset_frames: 40 }
    );
    let (resolved, seek_frame, offset_frames) =
        resolve_retarget_plan(&plan, RetargetSeekDecision::Requested { offset_frames: 40 });
    assert_eq!(resolved.seek_offset_frames(), 40);
    assert_eq!(seek_frame, Some(plan.start_frame() + 40));
    assert_eq!(offset_frames, 40);
}

#[test]
fn guarded_retarget_uses_adjacent_exact_frames_on_long_tracks() {
    let plan = span_plan_with(3_600.0, 1_600.0, 2_500.0, 0, 48_000);
    let first = 86_399_999;
    let second = first + 1;

    let first_decision = resolve_retarget_seek(&plan, false, true, Some(first));
    let second_decision = resolve_retarget_seek(&plan, false, true, Some(second));
    assert!(matches!(
        first_decision,
        RetargetSeekDecision::Preserve { .. }
    ));
    assert!(matches!(
        second_decision,
        RetargetSeekDecision::Preserve { .. }
    ));
    assert_eq!(
        first_decision,
        RetargetSeekDecision::Preserve {
            offset_frames: first - plan.start_frame()
        }
    );
    assert_eq!(
        second_decision,
        RetargetSeekDecision::Preserve {
            offset_frames: second - plan.start_frame()
        }
    );
}

#[test]
fn guarded_retarget_restarts_tiny_spans_and_clears_requested_seek_offset() {
    let plan = span_plan_with(1.0, 0.5, 0.5, 40, 1_000);
    assert_eq!(plan.frame_count(), 1);
    let decision = resolve_retarget_seek(&plan, false, true, Some(plan.start_frame()));
    assert_eq!(decision, RetargetSeekDecision::RestartAtStart);

    let (resolved, seek_frame, offset_frames) = resolve_retarget_plan(&plan, decision);
    assert_eq!(resolved.seek(), PlaybackSeekBehavior::SpanStart);
    assert_eq!(seek_frame, Some(plan.start_frame()));
    assert_eq!(offset_frames, 0);
}

#[test]
fn guarded_retarget_keeps_legacy_non_seeking_behavior_when_disabled() {
    let plan = span_plan(40);

    assert_eq!(
        resolve_retarget_seek(&plan, false, false, Some(plan.end_frame())),
        RetargetSeekDecision::Legacy { offset_frames: 0 }
    );
    assert_eq!(
        resolve_retarget_seek(&plan, false, false, None),
        RetargetSeekDecision::Legacy { offset_frames: 40 }
    );
}

#[test]
fn legacy_mode_restart_keeps_requested_offset_while_live_update_rebases_progress() {
    let plan = span_plan(40);
    let current_frame = plan.start_frame() + 325;
    let decision = resolve_retarget_seek(&plan, false, false, Some(current_frame));

    assert_eq!(
        decision,
        RetargetSeekDecision::Legacy { offset_frames: 325 }
    );
    assert_eq!(
        decision.restart_offset_frames(&plan),
        plan.seek_offset_frames()
    );
    assert_eq!(decision.replacement_offset_frames(&plan, false), 40);
    assert_eq!(decision.replacement_offset_frames(&plan, true), 325);

    let (_, seek_frame, live_offset) = resolve_retarget_plan(&plan, decision);
    assert_eq!(seek_frame, None);
    assert_eq!(live_offset, 325);
}

#[test]
fn converted_cached_replacement_preserves_guarded_and_explicit_seek_offsets() {
    let plan = span_plan(40);
    let current = plan.start_frame() + 325;
    let guarded = resolve_retarget_seek(&plan, false, true, Some(current));
    let requested = resolve_retarget_seek(&plan, true, true, Some(current));
    let outside = resolve_retarget_seek(&plan, false, true, Some(plan.end_frame()));
    assert_eq!(guarded.replacement_offset_frames(&plan, true), 325);
    assert_eq!(requested.replacement_offset_frames(&plan, true), 40);
    assert_eq!(outside.replacement_offset_frames(&plan, true), 0);
}
