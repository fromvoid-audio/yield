use std::path::PathBuf;

use crate::{
    PlaybackChannelLayout, PlaybackSeekBehavior, PlaybackSourceIdentity, PlaybackSourceKind,
    PlaybackSpanPlan, PlaybackSpanPlanError, PlaybackSpanRequest, PreparedTempoAudition,
    TempoAuditionFrameMap, timebase::frames_to_seconds,
};

use super::{
    PlaybackRuntimeGainNormalization, PlaybackRuntimeMode, PlaybackRuntimeProgress,
    interleaved_span_sample_bounds, normalized_gain_from_peak,
};

/// Prepared playback PCM bound to its original file coordinates.
#[derive(Clone, Debug)]
pub struct PlaybackRuntimeTempoSpan {
    pub(super) original_path: PathBuf,
    pub(super) prepared: PreparedTempoAudition,
    original_frames: u64,
    loop_start_frame: u64,
}

impl PlaybackRuntimeTempoSpan {
    /// Bind prepared PCM to a source file, its total frame count, and a loop start boundary.
    ///
    /// The caller has already validated the file and retains the ordinary playback request
    /// for fallback. The one-shot starts at the prepared span's first frame.
    pub fn new(
        original_path: PathBuf,
        prepared: PreparedTempoAudition,
        original_frames: u64,
        loop_start_frame: u64,
    ) -> Result<Self, String> {
        let output_frames = prepared.frames.output_frames();
        let source_span = prepared.frames.source_span();
        if source_span.end > original_frames
            || !(source_span.start..=source_span.end).contains(&loop_start_frame)
            || prepared.sample_rate == 0
            || !(1..=usize::from(u16::MAX)).contains(&prepared.channels)
            || output_frames.checked_mul(prepared.channels as u64)
                != Some(prepared.samples.len() as u64)
        {
            return Err(String::from(
                "Invalid prepared tempo playback layout or source span",
            ));
        }
        Ok(Self {
            original_path,
            prepared,
            original_frames,
            loop_start_frame,
        })
    }

    pub(super) fn timeline(&self) -> TempoPlaybackTimeline {
        TempoPlaybackTimeline {
            frames: self.prepared.frames.clone(),
            original_frames: self.original_frames,
        }
    }

    pub(super) fn playback_plan(
        &self,
        mode: PlaybackRuntimeMode,
    ) -> Result<PlaybackSpanPlan, PlaybackSpanPlanError> {
        let frames = self.prepared.frames.output_frames();
        let duration = frames_to_seconds(frames, self.prepared.sample_rate);
        let (looped, seek) = match mode {
            PlaybackRuntimeMode::OneShot { .. } => (false, PlaybackSeekBehavior::SpanStart),
            PlaybackRuntimeMode::Looped { .. } => (
                true,
                PlaybackSeekBehavior::FrameOffset(
                    self.prepared
                        .frames
                        .output_frame_for_source(self.loop_start_frame),
                ),
            ),
        };
        let layout =
            PlaybackChannelLayout::new(self.prepared.channels as u16, self.prepared.sample_rate)?;
        let source = PlaybackSourceIdentity::new(
            PlaybackSourceKind::Bytes,
            Some(self.prepared.samples.len() as u64),
        );
        let request = PlaybackSpanRequest::new(0.0, duration, duration, looped, seek)
            .with_exact_track_frames(frames);
        PlaybackSpanPlan::new(source, layout, request)
    }
}

pub(super) struct TempoPlaybackTimeline {
    frames: TempoAuditionFrameMap,
    original_frames: u64,
}

impl TempoPlaybackTimeline {
    pub(super) fn source_progress(&self, output_frame: u64) -> f32 {
        (self.frames.source_frame_for_output(output_frame) as f64 / self.original_frames as f64)
            as f32
    }

    pub(super) fn apply_progress(
        &self,
        progress: &mut PlaybackRuntimeProgress,
        frame: Option<u64>,
    ) {
        progress.progress = frame.map(|frame| self.source_progress(frame));
        if progress.active {
            let range = self.frames.source_span();
            progress.playback_span_frames = Some((range.start, range.end));
            progress.track_total_frames = Some(self.original_frames);
        }
    }

    pub(super) fn normalized_gain(
        &self,
        samples: &[f32],
        channels: usize,
        normalization: PlaybackRuntimeGainNormalization,
    ) -> Option<f32> {
        let bounds = interleaved_span_sample_bounds(
            self.original_frames.try_into().ok()?,
            1,
            normalization.start,
            normalization.end,
        )?;
        let start = self.frames.output_frame_for_source(bounds.start as u64) as usize * channels;
        let end = self.frames.output_frame_for_source(bounds.end as u64) as usize * channels;
        let span = samples.get(start..end)?;
        if span.is_empty() {
            return None;
        }
        let peak = span
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        Some(normalized_gain_from_peak(peak))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TempoAuditionTarget, prepare_tempo_audition};
    use std::{
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    fn prepared() -> PreparedTempoAudition {
        prepare_tempo_audition(
            TempoAuditionTarget::Enabled {
                source_bpm: Some(100.0),
                target_bpm: Some(150.0),
            },
            300..1_501,
            &[0.25, -0.125].repeat(1_201),
            12_000,
            2,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap()
    }

    #[test]
    fn prepared_tempo_runtime_uses_exact_one_shot_loop_and_source_progress() {
        let tempo =
            PlaybackRuntimeTempoSpan::new("original.wav".into(), prepared(), 3_000, 901).unwrap();
        let one_shot = tempo
            .playback_plan(PlaybackRuntimeMode::OneShot {
                start: 0.1,
                end: 0.51,
            })
            .unwrap();
        assert!(!one_shot.looped());
        assert_eq!(one_shot.track_frames(), 801);
        assert_eq!((one_shot.start_frame(), one_shot.end_frame()), (0, 801));
        assert_eq!(one_shot.sample_count(), 1_602);
        assert_eq!(one_shot.seek_sample(), 0);

        let loop_mode = PlaybackRuntimeMode::Looped {
            start: 0.1,
            end: 0.51,
            offset: 0.3,
        };
        let looped = tempo.playback_plan(loop_mode).unwrap();
        assert!(looped.looped());
        assert_eq!((looped.start_frame(), looped.end_frame()), (0, 801));
        assert_eq!(looped.seek_offset_frames(), 401);
        assert_eq!(looped.seek_sample(), 802);
        let timeline = tempo.timeline();
        let mut progress = PlaybackRuntimeProgress {
            active: true,
            looping: true,
            playback_span_frames: Some((0, 801)),
            track_total_frames: Some(801),
            ..Default::default()
        };
        timeline.apply_progress(&mut progress, Some(looped.seek_offset_frames()));
        assert_eq!(progress.playback_span_frames, Some((300, 1_501)));
        assert_eq!(progress.track_total_frames, Some(3_000));
        assert_eq!(progress.progress, Some(901.0 / 3_000.0));
        assert_eq!(timeline.source_progress(801), 1_501.0 / 3_000.0);

        let endpoint =
            PlaybackRuntimeTempoSpan::new("original.wav".into(), tempo.prepared, 3_000, 1_501)
                .unwrap();
        assert_eq!(endpoint.playback_plan(loop_mode).unwrap().seek_sample(), 0);
    }

    #[test]
    fn prepared_tempo_runtime_validates_layout_and_normalizes_only_the_mapped_span() {
        let pcm = prepared();
        assert!(
            PlaybackRuntimeTempoSpan::new("original.wav".into(), pcm.clone(), 1_500, 900).is_err()
        );
        assert!(
            PlaybackRuntimeTempoSpan::new("original.wav".into(), pcm.clone(), 3_000, 299).is_err()
        );
        let mut invalid = pcm.clone();
        invalid.channels = 3;
        assert!(PlaybackRuntimeTempoSpan::new("original.wav".into(), invalid, 3_000, 900).is_err());

        let tempo = PlaybackRuntimeTempoSpan::new("original.wav".into(), pcm, 3_000, 900).unwrap();
        let timeline = tempo.timeline();
        let normalize = |start, end| {
            timeline.normalized_gain(
                &tempo.prepared.samples,
                tempo.prepared.channels,
                PlaybackRuntimeGainNormalization::new(start, end),
            )
        };
        assert_eq!(normalize(0.0, 0.09), None);
        assert_eq!(normalize(0.2, 0.4), Some(4.0));
    }
}
