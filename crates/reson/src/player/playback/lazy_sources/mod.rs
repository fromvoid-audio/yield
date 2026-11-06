mod aiff_file;
mod decoder_source;
mod f32_cursor;
mod f32_file;
mod pcm_wav_cursor;
mod pcm_wav_file;
mod repeating_source;
mod span_source;

use std::time::Duration;

use super::super::PlaybackSpanPlan;
use crate::timebase::duration_for_frames;

pub(super) use aiff_file::AiffSpanSource;
pub(super) use f32_file::{InterleavedF32FileRepeatingSpanSource, InterleavedF32FileSpanSource};
pub(super) use pcm_wav_file::{PcmWavFileRepeatingSpanSource, PcmWavFileSpanSource};
pub(super) use repeating_source::LazyRepeatingSpanSource;
pub(super) use span_source::LazySpanSource;

#[derive(Clone, Copy)]
struct SourceFormat {
    sample_rate: u32,
    channels: u16,
}

impl SourceFormat {
    fn from_plan(plan: &PlaybackSpanPlan) -> Self {
        Self::new(plan.layout().sample_rate(), plan.layout().channels())
    }

    fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
        }
    }

    fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    fn channels(self) -> u16 {
        self.channels
    }
}

#[derive(Clone, Copy)]
struct SpanReadRequest {
    start_frame: u64,
    span_samples: u64,
    total_duration: Duration,
}

impl SpanReadRequest {
    fn from_plan(plan: &PlaybackSpanPlan) -> Self {
        let offset_frames = (!plan.looped())
            .then(|| plan.seek_offset_frames())
            .unwrap_or(0);
        let start_frame = plan.start_frame().saturating_add(offset_frames);
        let remaining_frames = plan.frame_count().saturating_sub(offset_frames).max(1);
        Self {
            start_frame,
            span_samples: remaining_frames.saturating_mul(plan.layout().channels() as u64),
            total_duration: duration_for_frames(plan.track_frames(), plan.layout().sample_rate()),
        }
    }
}

#[derive(Clone, Copy)]
struct RepeatReadRequest {
    start_frame: u64,
    span_samples: u64,
    offset_frames: u64,
}

impl RepeatReadRequest {
    fn from_plan(plan: &PlaybackSpanPlan) -> Self {
        Self {
            start_frame: plan.start_frame(),
            span_samples: plan.sample_count(),
            offset_frames: plan.seek_offset_frames(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{
        PlaybackChannelLayout, PlaybackSeekBehavior, PlaybackSourceIdentity, PlaybackSourceKind,
        PlaybackSpanRequest,
    };

    #[test]
    fn one_shot_span_request_applies_exact_initial_offset() {
        let plan = PlaybackSpanPlan::new(
            PlaybackSourceIdentity::new(PlaybackSourceKind::File, None),
            PlaybackChannelLayout::new(1, 1_000).expect("layout"),
            PlaybackSpanRequest::new(0.1, 0.5, 1.0, false, PlaybackSeekBehavior::FrameOffset(125)),
        )
        .expect("plan");

        let request = SpanReadRequest::from_plan(&plan);

        assert_eq!(request.start_frame, plan.start_frame() + 125);
        assert_eq!(request.span_samples, plan.frame_count() - 125);
    }

    #[test]
    fn one_shot_span_start_request_remains_unchanged() {
        let plan = PlaybackSpanPlan::new(
            PlaybackSourceIdentity::new(PlaybackSourceKind::File, None),
            PlaybackChannelLayout::new(1, 1_000).expect("layout"),
            PlaybackSpanRequest::new(0.1, 0.5, 1.0, false, PlaybackSeekBehavior::SpanStart),
        )
        .expect("plan");

        let request = SpanReadRequest::from_plan(&plan);

        assert_eq!(request.start_frame, plan.start_frame());
        assert_eq!(request.span_samples, plan.frame_count());
    }
}
