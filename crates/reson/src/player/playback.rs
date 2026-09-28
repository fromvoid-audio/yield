use crate::SamplesBuffer;
use crate::telemetry;
use crate::timebase::{frames_to_seconds, seconds_to_frames_round};
use crate::{AsyncSource, Source};

use super::metronome::MetronomeSource;
use super::playback_span_handle::plan_anchor_frame;
use super::{
    AudioPlaybackSource, AudioPlayer, EditFadeSource, PlaybackChannelLayout,
    PlaybackMetronomeConfig, PlaybackRuntimeReplacePolicy, PlaybackSeekBehavior,
    PlaybackSpanHandle, PlaybackSpanPlan, PlaybackSpanRequest,
};

use lazy_sources::{
    InterleavedF32FileRepeatingSpanSource, InterleavedF32FileSpanSource, LazyRepeatingSpanSource,
    LazySpanSource, PcmWavFileRepeatingSpanSource, PcmWavFileSpanSource,
};
mod lazy_sources;
mod loop_mode;
mod span_edge_fade;
mod span_samples;
use loop_mode::LiveLoopModeSource;
pub(super) use loop_mode::LoopModeHandle;
use span_edge_fade::StaticSpanEdgeFadeSource;
use span_samples::{SpanSamplesMode, SpanSamplesSource};
#[cfg(test)]
mod test_support;

impl AudioPlayer {
    /// Begin playback from the stored buffer.
    pub fn play(&mut self) -> Result<(), String> {
        self.play_range(0.0, 1.0, false)
    }

    /// Begin playback at the given normalized position (0.0 - 1.0).
    pub fn play_from_fraction(&mut self, fraction: f64) -> Result<(), String> {
        self.play_range(fraction, 1.0, false)
    }

    /// Play between two normalized points, optionally looping the segment.
    pub fn play_range(&mut self, start: f64, end: f64, looped: bool) -> Result<(), String> {
        self.play_range_with_metronome(start, end, looped, None)
    }

    /// Play between two normalized points with an optional click track.
    pub fn play_range_with_metronome(
        &mut self,
        start: f64,
        end: f64,
        looped: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.play_range_with_metronome_and_replace_policy(
            start,
            end,
            looped,
            PlaybackRuntimeReplacePolicy::FadeOutPrevious,
            metronome,
        )
    }

    pub(super) fn play_range_with_metronome_and_replace_policy(
        &mut self,
        start: f64,
        end: f64,
        looped: bool,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let (bounded_start, bounded_end, duration) = self.normalized_span(start, end)?;
        self.loop_offset = None;
        self.loop_offset_frames = None;
        self.start_with_span(
            bounded_start,
            bounded_end,
            duration,
            looped,
            replace_policy,
            metronome,
        )
    }

    /// Loop a selection while starting playback at an offset within the selection.
    pub fn play_looped_range_from(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
    ) -> Result<(), String> {
        self.play_looped_range_from_with_metronome(start, end, offset, None)
    }

    /// Loop a selection with an optional click track while starting playback at
    /// an offset inside the selection.
    pub fn play_looped_range_from_with_metronome(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.play_looped_range_from_with_metronome_and_replace_policy(
            start,
            end,
            offset,
            PlaybackRuntimeReplacePolicy::FadeOutPrevious,
            metronome,
        )
    }

    pub(super) fn play_looped_range_from_with_metronome_and_replace_policy(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let (bounded_start, bounded_end, duration) = self.normalized_span(start, end)?;
        let clamped_offset = offset.clamp(start.min(end), start.max(end));
        let offset_seconds =
            ((clamped_offset * f64::from(duration)) - f64::from(bounded_start)).max(0.0) as f32;
        self.start_with_looped_span_offset(
            bounded_start,
            bounded_end,
            duration,
            offset_seconds,
            replace_policy,
            metronome,
        )
    }

    /// Retarget the active loop span without rebuilding the sink when the
    /// current source supports live loop bounds.
    pub fn retarget_looped_range_with_metronome(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        seek_to_offset: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.retarget_looped_range_with_metronome_and_seek_policy(
            start,
            end,
            offset,
            seek_to_offset,
            false,
            metronome,
        )
    }

    /// Retarget an active loop span with an exact-frame outside-span seek policy.
    pub(super) fn retarget_looped_range_with_metronome_and_seek_policy(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        seek_to_offset: bool,
        seek_if_outside: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.retarget_playback_span_with_metronome(
            start,
            end,
            offset,
            seek_to_offset,
            seek_if_outside,
            true,
            metronome,
        )
    }

    /// Retarget an active one-shot span without rebuilding the sink when the
    /// current source supports live finite-span bounds.
    pub fn retarget_one_shot_range_with_metronome(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        seek_to_offset: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.retarget_playback_span_with_metronome(
            start,
            end,
            offset,
            seek_to_offset,
            false,
            false,
            metronome,
        )
    }

    /// Retarget an active one-shot span with an exact-frame outside-span seek policy.
    pub(super) fn retarget_one_shot_range_with_metronome_and_seek_policy(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        seek_to_offset: bool,
        seek_if_outside: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.retarget_playback_span_with_metronome(
            start,
            end,
            offset,
            seek_to_offset,
            seek_if_outside,
            false,
            metronome,
        )
    }

    fn retarget_playback_span_with_metronome(
        &mut self,
        start: f64,
        end: f64,
        offset: f64,
        seek_to_offset: bool,
        seek_if_outside: bool,
        looped: bool,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let (bounded_start, bounded_end, duration) = self.normalized_span(start, end)?;
        let clamped_offset = offset.clamp(start.min(end), start.max(end));
        let offset_seconds =
            ((clamped_offset * f64::from(duration)) - f64::from(bounded_start)).max(0.0) as f32;
        let sample_rate = self.sample_rate.unwrap_or(44_100).max(1);
        let offset_frames = seconds_to_frames_round(offset_seconds, sample_rate);
        let requested_plan = self.playback_span_plan(
            bounded_start,
            bounded_end,
            duration,
            looped,
            PlaybackSeekBehavior::FrameOffset(offset_frames),
        )?;
        // Take one source-frame snapshot before any mode restart or live control
        // update. The f32 progress ratio intentionally is not used by the
        // guarded policy because it aliases adjacent frames on long sources.
        let current_frame = (!seek_to_offset).then(|| self.progress_frame()).flatten();
        let decision = resolve_retarget_seek(
            &requested_plan,
            seek_to_offset,
            seek_if_outside,
            current_frame,
        );

        if self.looping != looped {
            // A mode-only change keeps the same decoded stream and audible
            // frame. The outer source stops at the next cycle end when the
            // loop is disabled; no second decoder or sink is started.
            if !seek_to_offset
                && self.play_span_frames
                    == Some((requested_plan.start_frame(), requested_plan.end_frame()))
                && self.is_playing()
                && self.active_metronome == metronome
                && let Some(mode) = self.active_loop_mode.as_ref()
                && mode.set_looped(looped)
            {
                if let Some(span) = self.active_playback_span.as_ref() {
                    span.update_from_plan(&requested_plan, None, metronome);
                }
                if self.active_playback_span.is_some()
                    && let Some(frame) = current_frame
                {
                    self.started_at = Some(std::time::Instant::now());
                    let offset = frame.saturating_sub(requested_plan.start_frame());
                    self.loop_offset_frames = Some(offset);
                    self.loop_offset = Some(frames_to_seconds(offset, sample_rate));
                }
                self.looping = looped;
                return Ok(());
            }
            return if looped {
                self.start_with_looped_span_offset_frames(
                    bounded_start,
                    bounded_end,
                    duration,
                    decision.restart_offset_frames(&requested_plan),
                    PlaybackRuntimeReplacePolicy::FadeOutPrevious,
                    metronome,
                )
            } else {
                self.start_with_span_offset_frames(
                    bounded_start,
                    bounded_end,
                    duration,
                    decision.restart_offset_frames(&requested_plan),
                    metronome,
                )
            };
        }

        let Some(playback_span) = self.active_playback_span.clone() else {
            return if looped {
                self.start_with_looped_span_offset_frames(
                    bounded_start,
                    bounded_end,
                    duration,
                    decision.restart_offset_frames(&requested_plan),
                    PlaybackRuntimeReplacePolicy::FadeOutPrevious,
                    metronome,
                )
            } else {
                self.start_with_span_offset_frames(
                    bounded_start,
                    bounded_end,
                    duration,
                    decision.restart_offset_frames(&requested_plan),
                    metronome,
                )
            };
        };
        let (plan, seek_frame, progress_offset_frames) =
            resolve_retarget_plan(&requested_plan, decision);
        playback_span.update_from_plan(&plan, seek_frame, metronome);
        self.finish_span_playback(&plan, Some(progress_offset_frames));
        self.active_playback_span = Some(playback_span);
        self.active_metronome = metronome;
        Ok(())
    }

    /// Loop the full track while starting playback at the given normalized position.
    pub fn play_full_wrapped_from(&mut self, start: f64) -> Result<(), String> {
        let duration = self
            .track_duration
            .ok_or_else(|| "Load a .wav file first".to_string())?;
        if duration <= 0.0 {
            return Err("Load a .wav file first".into());
        }

        self.fade_out_current_sink(self.anti_clip_fade());

        let sample_rate = self.sample_rate.unwrap_or(44_100).max(1);
        let channels = self.track_channels.unwrap_or(1).max(1);

        let total_frames = self
            .track_total_frames
            .unwrap_or_else(|| seconds_to_frames_round(duration, sample_rate).max(1));
        let plan_duration = frames_to_seconds(total_frames, sample_rate);
        let offset_frame = ((start.clamp(0.0, 1.0) * total_frames as f64).floor() as u64)
            .min(total_frames.saturating_sub(1));
        let plan = self.playback_span_plan(
            0.0,
            plan_duration,
            plan_duration,
            true,
            PlaybackSeekBehavior::FrameOffset(offset_frame),
        )?;
        let diagnostic: Box<dyn Source<Item = f32> + Send> = if let Some(samples) =
            self.playback_samples.as_ref().cloned()
        {
            let source = SamplesBuffer::from_arc_span_at(
                channels,
                sample_rate,
                samples,
                0,
                plan.sample_count() as usize,
                plan.seek_sample(),
            )
            .repeat_infinite();
            Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                source,
                plan.sample_count(),
            ))
        } else {
            let loop_source = repeating_source_for_audio_source(self.audio_source()?, &plan)?;
            let mut async_source =
                AsyncSource::with_buffer_seconds(loop_source, self.stream_policy.buffer_seconds);
            async_source.prefill_for_duration(
                self.stream_policy.prefill_duration,
                self.stream_policy.prefill_timeout,
            );
            Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                async_source,
                plan.sample_count(),
            ))
        };

        let (handle, format) =
            self.build_sink_with_fade(diagnostic, PlaybackRuntimeReplacePolicy::FadeOutPrevious)?;
        self.finish_span_playback(&plan, Some(plan.seek_offset_frames()));
        self.active_playback_span = None;
        self.active_loop_mode = None;
        self.active_metronome = None;
        self.fade_out = Some(handle);
        self.sink_format = Some(format);
        Ok(())
    }

    fn start_with_span(
        &mut self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        looped: bool,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        self.start_with_span_plan(
            start_seconds,
            end_seconds,
            duration,
            looped,
            replace_policy,
            metronome,
            None,
            false,
        )
    }

    fn start_with_span_plan(
        &mut self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        looped: bool,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
        planned: Option<PlaybackSpanPlan>,
        transition_fade_in: bool,
    ) -> Result<(), String> {
        let total_started_at = playback_stage_started();
        let source_kind = self.current_source_kind();
        if duration <= 0.0 {
            return Err("Load a .wav file first".into());
        }

        let plan_started_at = playback_stage_started();
        let plan = planned.unwrap_or(self.playback_span_plan(
            start_seconds,
            end_seconds,
            duration,
            looped,
            PlaybackSeekBehavior::SpanStart,
        )?);
        log_playback_stage("span_plan", plan_started_at, source_kind, looped);
        let sample_rate = plan.layout().sample_rate();
        let channels = plan.layout().channels();

        let source_started_at = playback_stage_started();
        let mut active_playback_span = None;
        let mode = LoopModeHandle::new(looped, plan.frame_count(), plan.seek_offset_frames());
        let final_source: Box<dyn Source<Item = f32> + Send> = if looped {
            let diagnostic: Box<dyn Source<Item = f32> + Send> = if let Some(samples) =
                self.playback_samples.as_ref().cloned()
            {
                let playback_span = PlaybackSpanHandle::from_plan_with_metronome(&plan, metronome);
                let source = SpanSamplesSource::new(
                    channels,
                    sample_rate,
                    samples,
                    playback_span.clone(),
                    SpanSamplesMode::Looped,
                    plan.start_sample(),
                )
                .with_edge_fade(self.anti_clip_fade())
                .with_live_loop_mode(mode.clone());
                active_playback_span = Some(playback_span);
                Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                    source,
                    plan.sample_count(),
                ))
            } else {
                let loop_source = repeating_source_for_audio_source(self.audio_source()?, &plan)?;
                let mut async_source = AsyncSource::with_buffer_seconds(
                    loop_source,
                    self.stream_policy.buffer_seconds,
                );
                async_source.prefill_for_duration(
                    self.stream_policy.prefill_duration,
                    self.stream_policy.prefill_timeout,
                );
                let faded =
                    StaticSpanEdgeFadeSource::new(async_source, &plan, self.anti_clip_fade());
                Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                    faded,
                    plan.sample_count(),
                ))
            };
            let editable = EditFadeSource::new_looped(
                diagnostic,
                self.edit_fade_handle.clone(),
                plan.start_seconds(),
                plan.frame_count(),
                0,
            );
            Box::new(editable)
        } else {
            let source: Box<dyn Source<Item = f32> + Send> = if let Some(samples) =
                self.playback_samples.as_ref().cloned()
            {
                let playback_span = PlaybackSpanHandle::from_plan_with_metronome(&plan, metronome);
                let source = SpanSamplesSource::new(
                    channels,
                    sample_rate,
                    samples,
                    playback_span.clone(),
                    SpanSamplesMode::OneShot,
                    plan.seek_sample(),
                )
                .with_edge_fade(self.anti_clip_fade())
                .with_live_loop_mode(mode.clone());
                active_playback_span = Some(playback_span);
                Box::new(source)
            } else {
                let lazy_source = repeating_source_for_audio_source(self.audio_source()?, &plan)?;
                let mut async_source = AsyncSource::with_buffer_seconds(
                    lazy_source,
                    self.stream_policy.buffer_seconds,
                );
                async_source.prefill_for_duration(
                    self.stream_policy.prefill_duration,
                    self.stream_policy.prefill_timeout,
                );
                Box::new(StaticSpanEdgeFadeSource::new(
                    async_source,
                    &plan,
                    self.anti_clip_fade(),
                ))
            };
            let editable = EditFadeSource::new_looped(
                source,
                self.edit_fade_handle.clone(),
                plan.start_seconds(),
                plan.frame_count(),
                plan.seek_offset_frames(),
            );
            Box::new(editable)
        };
        let final_source =
            source_with_metronome(final_source, metronome, &plan, active_playback_span.clone());
        let final_source: Box<dyn Source<Item = f32> + Send> = if self.playback_samples.is_none() {
            Box::new(LiveLoopModeSource::new(final_source, mode.clone()))
        } else {
            final_source
        };
        // A live retarget may restart from the current frame. The span-edge fade is
        // already behind that frame, so ramp the new source in as the old one
        // fades out instead of briefly summing two full-level streams.
        let final_source: Box<dyn Source<Item = f32> + Send> = if transition_fade_in {
            Box::new(final_source.fade_in(self.anti_clip_fade()))
        } else {
            final_source
        };
        log_playback_stage(
            "source_construction",
            source_started_at,
            source_kind,
            looped,
        );

        // File-backed sources may block while preparing. Keep the old source
        // audible until the replacement is ready to append.
        let clear_started_at = playback_stage_started();
        self.prepare_previous_sink_for_new_source(self.anti_clip_fade(), replace_policy);
        log_playback_stage(
            "clear_or_fade_current",
            clear_started_at,
            source_kind,
            looped,
        );
        let (handle, format) = self.build_sink_with_fade(final_source, replace_policy)?;
        let finish_started_at = playback_stage_started();
        let initial_offset = (!looped).then_some(plan.seek_offset_frames());
        self.finish_span_playback(&plan, initial_offset);
        self.active_playback_span = active_playback_span;
        self.active_loop_mode = Some(mode);
        self.active_metronome = metronome;
        self.fade_out = Some(handle);
        self.sink_format = Some(format);
        log_playback_stage("finish_span_state", finish_started_at, source_kind, looped);
        log_playback_stage(
            "start_with_span_total",
            total_started_at,
            source_kind,
            looped,
        );
        Ok(())
    }

    fn start_with_span_offset_frames(
        &mut self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        offset_frames: u64,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let plan = self.playback_span_plan(
            start_seconds,
            end_seconds,
            duration,
            false,
            PlaybackSeekBehavior::FrameOffset(offset_frames),
        )?;
        self.start_with_span_plan(
            start_seconds,
            end_seconds,
            duration,
            false,
            PlaybackRuntimeReplacePolicy::FadeOutPrevious,
            metronome,
            Some(plan),
            true,
        )
    }

    fn start_with_looped_span_offset(
        &mut self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        offset_seconds: f32,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let sample_rate = self.sample_rate.unwrap_or(44_100).max(1);
        let offset_frames = seconds_to_frames_round(offset_seconds, sample_rate);
        self.start_with_looped_span_offset_frames(
            start_seconds,
            end_seconds,
            duration,
            offset_frames,
            replace_policy,
            metronome,
        )
    }

    fn start_with_looped_span_offset_frames(
        &mut self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        offset_frames: u64,
        replace_policy: PlaybackRuntimeReplacePolicy,
        metronome: Option<PlaybackMetronomeConfig>,
    ) -> Result<(), String> {
        let total_started_at = playback_stage_started();
        let source_kind = self.current_source_kind();
        if duration <= 0.0 {
            return Err("Load a .wav file first".into());
        }

        let clear_started_at = playback_stage_started();
        self.prepare_previous_sink_for_new_source(self.anti_clip_fade(), replace_policy);
        log_playback_stage("clear_or_fade_current", clear_started_at, source_kind, true);

        let plan_started_at = playback_stage_started();
        let plan = self.playback_span_plan(
            start_seconds,
            end_seconds,
            duration,
            true,
            PlaybackSeekBehavior::FrameOffset(offset_frames),
        )?;
        log_playback_stage("span_plan", plan_started_at, source_kind, true);
        let sample_rate = plan.layout().sample_rate();
        let channels = plan.layout().channels();

        let source_started_at = playback_stage_started();
        let mut active_playback_span = None;
        let mode = LoopModeHandle::new(true, plan.frame_count(), plan.seek_offset_frames());
        let diagnostic: Box<dyn Source<Item = f32> + Send> = if let Some(samples) =
            self.playback_samples.as_ref().cloned()
        {
            let playback_span = PlaybackSpanHandle::from_plan_with_metronome(&plan, metronome);
            let source = SpanSamplesSource::new(
                channels,
                sample_rate,
                samples,
                playback_span.clone(),
                SpanSamplesMode::Looped,
                plan.seek_sample(),
            )
            .with_edge_fade(self.anti_clip_fade())
            .with_live_loop_mode(mode.clone());
            let editable = EditFadeSource::new_looped(
                source,
                self.edit_fade_handle.clone(),
                plan.start_seconds(),
                plan.frame_count(),
                plan.seek_offset_frames(),
            );
            active_playback_span = Some(playback_span);
            Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                editable,
                plan.sample_count(),
            ))
        } else {
            let loop_source = repeating_source_for_audio_source(self.audio_source()?, &plan)?;
            let mut async_source =
                AsyncSource::with_buffer_seconds(loop_source, self.stream_policy.buffer_seconds);
            async_source.prefill_for_duration(
                self.stream_policy.prefill_duration,
                self.stream_policy.prefill_timeout,
            );
            let faded = StaticSpanEdgeFadeSource::new(async_source, &plan, self.anti_clip_fade());
            let editable = EditFadeSource::new_looped(
                faded,
                self.edit_fade_handle.clone(),
                plan.start_seconds(),
                plan.frame_count(),
                plan.seek_offset_frames(),
            );
            Box::new(crate::loop_diagnostic::LoopDiagnostic::new(
                editable,
                plan.sample_count(),
            ))
        };
        let diagnostic =
            source_with_metronome(diagnostic, metronome, &plan, active_playback_span.clone());
        let diagnostic: Box<dyn Source<Item = f32> + Send> = if self.playback_samples.is_none() {
            Box::new(LiveLoopModeSource::new(diagnostic, mode.clone()))
        } else {
            diagnostic
        };
        log_playback_stage("source_construction", source_started_at, source_kind, true);

        let (handle, format) = self.build_sink_with_fade(diagnostic, replace_policy)?;
        let finish_started_at = playback_stage_started();
        self.finish_span_playback(&plan, Some(plan.seek_offset_frames()));
        self.active_playback_span = active_playback_span;
        self.active_loop_mode = Some(mode);
        self.active_metronome = metronome;
        self.fade_out = Some(handle);
        self.sink_format = Some(format);
        log_playback_stage("finish_span_state", finish_started_at, source_kind, true);
        log_playback_stage(
            "start_with_looped_span_total",
            total_started_at,
            source_kind,
            true,
        );
        Ok(())
    }

    fn playback_span_plan(
        &self,
        start_seconds: f32,
        end_seconds: f32,
        duration: f32,
        looped: bool,
        seek: PlaybackSeekBehavior,
    ) -> Result<PlaybackSpanPlan, String> {
        let source = self.audio_source()?;
        let layout = PlaybackChannelLayout::new(
            self.track_channels.unwrap_or(1).max(1),
            self.sample_rate.unwrap_or(44_100).max(1),
        )
        .map_err(|err| err.to_string())?;
        PlaybackSpanPlan::new(
            source.identity(),
            layout,
            PlaybackSpanRequest::new(start_seconds, end_seconds, duration, looped, seek),
        )
        .map_err(|err| err.to_string())
    }

    fn finish_span_playback(&mut self, span: &PlaybackSpanPlan, offset_frames: Option<u64>) {
        let sample_rate = span.layout().sample_rate();
        self.started_at = Some(std::time::Instant::now());
        self.play_span = Some((span.start_seconds(), span.end_seconds()));
        self.play_span_frames = Some((span.start_frame(), span.end_frame()));
        self.looping = span.looped();
        self.loop_offset = offset_frames.map(|frames| frames_to_seconds(frames, sample_rate));
        self.loop_offset_frames = offset_frames;
        self.track_duration = Some(frames_to_seconds(span.track_frames(), sample_rate));
        self.track_total_frames = Some(span.track_frames());
        self.sample_rate = Some(sample_rate);
        #[cfg(test)]
        {
            self.elapsed_override = None;
        }
    }

    fn current_source_kind(&self) -> &'static str {
        self.current_audio
            .as_ref()
            .map(AudioPlaybackSource::kind)
            .unwrap_or("none")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetargetSeekDecision {
    /// Honor an explicit seek request at the requested frame offset.
    Requested { offset_frames: u64 },
    /// Keep the exact current source frame in the new span.
    Preserve { offset_frames: u64 },
    /// Restart at the frame-quantized start of the new span.
    RestartAtStart,
    /// Preserve the legacy non-seeking behavior when no outside-span policy is requested.
    Legacy { offset_frames: u64 },
}

impl RetargetSeekDecision {
    fn restart_offset_frames(self, plan: &PlaybackSpanPlan) -> u64 {
        match self {
            Self::Requested { offset_frames } | Self::Preserve { offset_frames } => {
                offset_frames.min(plan.frame_count().saturating_sub(1))
            }
            Self::RestartAtStart => 0,
            Self::Legacy { .. } => plan.seek_offset_frames(),
        }
    }
}

fn resolve_retarget_seek(
    plan: &PlaybackSpanPlan,
    seek_to_offset: bool,
    seek_if_outside: bool,
    current_frame: Option<u64>,
) -> RetargetSeekDecision {
    if seek_to_offset {
        return RetargetSeekDecision::Requested {
            offset_frames: plan.seek_offset_frames(),
        };
    }

    if seek_if_outside {
        return match current_frame {
            Some(frame) if retarget_frame_is_inside(plan, frame) => {
                RetargetSeekDecision::Preserve {
                    offset_frames: frame.saturating_sub(plan.start_frame()),
                }
            }
            _ => RetargetSeekDecision::RestartAtStart,
        };
    }

    RetargetSeekDecision::Legacy {
        offset_frames: live_retarget_progress_offset_frames(plan, false, current_frame),
    }
}

fn resolve_retarget_plan(
    requested_plan: &PlaybackSpanPlan,
    decision: RetargetSeekDecision,
) -> (PlaybackSpanPlan, Option<u64>, u64) {
    match decision {
        RetargetSeekDecision::Requested { .. } => (
            *requested_plan,
            Some(
                requested_plan
                    .start_frame()
                    .saturating_add(requested_plan.seek_offset_frames())
                    .min(requested_plan.end_frame().saturating_sub(1)),
            ),
            requested_plan.seek_offset_frames(),
        ),
        RetargetSeekDecision::Preserve { offset_frames } => (
            *requested_plan,
            None,
            offset_frames.min(requested_plan.frame_count().saturating_sub(1)),
        ),
        RetargetSeekDecision::RestartAtStart => {
            let plan = requested_plan.with_seek(PlaybackSeekBehavior::SpanStart);
            (plan, Some(plan.start_frame()), 0)
        }
        RetargetSeekDecision::Legacy { offset_frames } => (
            *requested_plan,
            None,
            offset_frames.min(requested_plan.frame_count().saturating_sub(1)),
        ),
    }
}

fn retarget_frame_is_inside(plan: &PlaybackSpanPlan, frame: u64) -> bool {
    let guard_frames = plan.track_frames().div_ceil(100);
    frame >= plan.start_frame() && frame.saturating_add(guard_frames) < plan.end_frame()
}

fn live_retarget_progress_offset_frames(
    plan: &PlaybackSpanPlan,
    seek_to_offset: bool,
    current_frame: Option<u64>,
) -> u64 {
    if seek_to_offset {
        return plan.seek_offset_frames();
    }
    match current_frame {
        Some(frame) if (plan.start_frame()..plan.end_frame()).contains(&frame) => {
            frame.saturating_sub(plan.start_frame())
        }
        Some(_) => 0,
        None => plan.seek_offset_frames(),
    }
}

#[cfg(test)]
mod retarget_progress_tests;

fn source_with_metronome(
    source: Box<dyn Source<Item = f32> + Send>,
    metronome: Option<PlaybackMetronomeConfig>,
    plan: &PlaybackSpanPlan,
    live_control: Option<PlaybackSpanHandle>,
) -> Box<dyn Source<Item = f32> + Send> {
    match (live_control, metronome) {
        (Some(control), _) => Box::new(MetronomeSource::new_live(source, control)),
        (None, Some(config)) => Box::new(MetronomeSource::new(
            source,
            config,
            plan.frame_count(),
            plan.seek_offset_frames(),
            plan_anchor_frame(plan),
        )),
        (None, None) => source,
    }
}

fn playback_stage_started() -> Option<std::time::Instant> {
    telemetry::playback_telemetry_enabled().then(std::time::Instant::now)
}

fn log_playback_stage(
    stage: &'static str,
    started_at: Option<std::time::Instant>,
    source_kind: &'static str,
    looped: bool,
) {
    let Some(started_at) = started_at else {
        return;
    };
    tracing::info!(
        target: "perf::audio_start",
        module = "reson_player",
        stage,
        source_kind,
        looped,
        elapsed_ms = telemetry::elapsed_ms(started_at.elapsed()),
        "Audio player stage"
    );
}

fn span_source_for_audio_source(
    source: AudioPlaybackSource,
    plan: &PlaybackSpanPlan,
) -> Result<Box<dyn Source<Item = f32> + Send>, String> {
    match source {
        AudioPlaybackSource::InterleavedF32File { path, sample_count } => Ok(Box::new(
            InterleavedF32FileSpanSource::new(path, plan, sample_count),
        )),
        AudioPlaybackSource::File(path) => {
            match PcmWavFileSpanSource::try_new(path.clone(), plan) {
                Ok(source) => Ok(Box::new(source)),
                Err(_) => Ok(Box::new(LazySpanSource::new(
                    AudioPlaybackSource::File(path),
                    plan,
                ))),
            }
        }
        source => Ok(Box::new(LazySpanSource::new(source, plan))),
    }
}

fn repeating_source_for_audio_source(
    source: AudioPlaybackSource,
    plan: &PlaybackSpanPlan,
) -> Result<Box<dyn Source<Item = f32> + Send>, String> {
    match source {
        AudioPlaybackSource::InterleavedF32File { path, sample_count } => Ok(Box::new(
            InterleavedF32FileRepeatingSpanSource::new(path, plan, sample_count),
        )),
        AudioPlaybackSource::File(path) => {
            match PcmWavFileRepeatingSpanSource::try_new(path.clone(), plan) {
                Ok(source) => Ok(Box::new(source)),
                Err(_) => Ok(Box::new(LazyRepeatingSpanSource::new(
                    AudioPlaybackSource::File(path),
                    plan,
                ))),
            }
        }
        source => Ok(Box::new(LazyRepeatingSpanSource::new(source, plan))),
    }
}
