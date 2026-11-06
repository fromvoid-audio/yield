use std::time::Duration;

use super::super::routing::{duration_from_secs_f32, duration_mod};
use super::super::timebase::duration_for_frames;
use super::super::timebase::duration_to_frames_floor;
use super::AudioPlayer;

impl AudioPlayer {
    /// Current playback progress as a 0-1 fraction.
    pub fn progress(&self) -> Option<f32> {
        let duration = self.track_duration?;
        if duration <= 0.0 {
            return None;
        }

        if let (Some(track_frames), Some(absolute_frame)) =
            (self.track_total_frames, self.progress_frame())
        {
            if track_frames == 0 {
                return None;
            }
            return Some(((absolute_frame as f64 / track_frames as f64) as f32).clamp(0.0, 1.0));
        }

        let started_at = self.started_at?;
        let elapsed = self.elapsed_since(started_at);
        let (span_start, span_end) = self.play_span.unwrap_or((0.0, duration));
        let span_length_secs = (span_end - span_start).max(f32::EPSILON);
        let span_length = duration_from_secs_f32(span_length_secs);
        if span_length.is_zero() {
            return None;
        }
        let base_offset = duration_from_secs_f32(self.loop_offset.unwrap_or(0.0)).min(span_length);
        let within_span = if self.looping {
            duration_mod(base_offset.saturating_add(elapsed), span_length)
        } else {
            base_offset.saturating_add(elapsed).min(span_length)
        };
        let absolute_secs = span_start as f64 + within_span.as_secs_f64();
        Some(((absolute_secs / duration as f64) as f32).clamp(0.0, 1.0))
    }

    pub(super) fn progress_frame(&self) -> Option<u64> {
        let started_at = self.started_at?;
        let sample_rate = self.sample_rate?;
        let track_frames = self.track_total_frames?;
        let (span_start, span_end) = self.play_span_frames?;
        let span_frames = span_end.saturating_sub(span_start).max(1);
        if (self.active_playback_span.is_none() || self.rendered_source_position.is_some())
            && let Some(mode) = self.active_loop_mode.as_ref()
        {
            let within_span = mode.progress_offset();
            return Some(span_start.saturating_add(within_span).min(track_frames));
        }
        let elapsed_frames = self.elapsed_source_frames(started_at, sample_rate);
        let within_span = position_in_span(
            self.loop_offset_frames.unwrap_or(0).min(span_frames),
            elapsed_frames,
            span_frames,
            self.looping,
        );
        Some(span_start.saturating_add(within_span).min(track_frames))
    }

    /// True while the sink is still playing the queued audio.
    pub fn is_playing(&self) -> bool {
        self.stream.active_source_count() > 0 && self.started_at.is_some()
    }

    /// Wall-clock elapsed time since the current playback span was started.
    pub fn playback_elapsed(&self) -> Option<Duration> {
        self.started_at
            .map(|started_at| self.elapsed_since(started_at))
    }

    fn elapsed_source_frames(&self, started_at: std::time::Instant, sample_rate: u32) -> u64 {
        self.rendered_source_position.as_ref().map_or_else(
            || duration_to_frames_floor(self.elapsed_since(started_at), sample_rate),
            |position| position.source_frame(),
        )
    }

    /// True when the current sink is configured to loop.
    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// Return the configured beat count for the active sample metronome.
    ///
    /// Returns `None` when playback is inactive, the metronome is disabled, or
    /// the active source does not expose a live playback span.
    pub fn metronome_beat_count(&self) -> Option<u16> {
        if !self.is_playing() {
            return None;
        }
        self.active_playback_span
            .as_ref()?
            .current_metronome_beat_count()
    }

    /// Return the active playback span in seconds.
    #[doc(hidden)]
    pub fn play_span(&self) -> Option<(f32, f32)> {
        self.play_span
    }

    /// Return the exact frame bounds of the configured playback span.
    #[doc(hidden)]
    pub(super) fn playback_span_frames(&self) -> Option<(u64, u64)> {
        self.play_span_frames
    }

    /// Return the exact number of source frames used by the playback plan.
    #[doc(hidden)]
    pub(super) fn track_total_frames(&self) -> Option<u64> {
        self.track_total_frames
    }

    /// Return the loaded track duration in seconds.
    #[doc(hidden)]
    pub fn track_duration(&self) -> Option<f32> {
        self.track_duration
    }

    /// Return the current normalized playback gain.
    #[doc(hidden)]
    pub fn playback_gain_for_tests(&self) -> f32 {
        self.playback_gain
    }

    /// Remaining wall-clock time until the current loop iteration finishes.
    pub fn remaining_loop_duration(&self) -> Option<Duration> {
        if !self.looping {
            return None;
        }
        let started_at = self.started_at?;
        if let (Some(sample_rate), Some((span_start, span_end))) =
            (self.sample_rate, self.play_span_frames)
        {
            let span_frames = span_end.saturating_sub(span_start).max(1);
            let elapsed_frames = self.elapsed_source_frames(started_at, sample_rate);
            let elapsed_in_span = position_in_span(
                self.loop_offset_frames.unwrap_or(0),
                elapsed_frames,
                span_frames,
                true,
            );
            let remaining_frames = span_frames.saturating_sub(elapsed_in_span);
            return Some(duration_for_frames(remaining_frames, sample_rate));
        }

        let (start, end) = self.play_span?;
        let span_length_secs = (end - start).max(f32::EPSILON);
        let span_length = duration_from_secs_f32(span_length_secs);
        if span_length.is_zero() {
            return None;
        }
        let elapsed = self.elapsed_since(started_at);
        let base_offset = duration_from_secs_f32(self.loop_offset.unwrap_or(0.0));
        let elapsed_in_span = duration_mod(base_offset.saturating_add(elapsed), span_length);
        Some(span_length.saturating_sub(elapsed_in_span))
    }

    /// Returns and clears the last error from the audio stream.
    pub fn take_error(&mut self) -> Option<String> {
        self.stream.take_error()
    }
}

fn position_in_span(offset: u64, elapsed: u64, span_frames: u64, looped: bool) -> u64 {
    if looped {
        (offset % span_frames).saturating_add(elapsed) % span_frames
    } else {
        offset.saturating_add(elapsed).min(span_frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{AudioOutputConfig, open_output_stream};
    use crate::timebase::{duration_for_frames, duration_to_frames_floor};
    use std::time::Instant;

    #[test]
    fn rendered_source_frames_wrap_at_loop_boundary_and_clamp_at_one_shot_end() {
        assert_eq!(super::position_in_span(10, 0, 147, false), 10);
        assert_eq!(super::position_in_span(10, 136, 147, true), 146);
        assert_eq!(super::position_in_span(10, 137, 147, true), 0);
        assert_eq!(super::position_in_span(10, 138, 147, true), 1);
        assert_eq!(super::position_in_span(10, 138, 147, false), 147);
    }

    #[test]
    fn progress_uses_frame_timebase_when_available() {
        let Ok(outcome) = open_output_stream(&AudioOutputConfig::default()) else {
            return;
        };
        let stream = outcome.stream;
        let mut player = AudioPlayer::test_with_state(
            stream,
            Some(1.0),
            Some(Instant::now()),
            Some((0.0, 1.0)),
            false,
            None,
            Some(duration_for_frames(12_000, 48_000)),
        );
        player.sample_rate = Some(48_000);
        player.track_total_frames = Some(48_000);
        player.play_span_frames = Some((0, 48_000));

        let progress = player.progress().expect("progress");
        let expected = 12_000.0 / 48_000.0;
        assert!((progress - expected).abs() < 1e-6);
    }

    #[test]
    fn configured_span_readback_preserves_exact_source_frames() {
        let Ok(outcome) = open_output_stream(&AudioOutputConfig::default()) else {
            return;
        };
        let stream = outcome.stream;
        let mut player = AudioPlayer::test_with_state(
            stream,
            Some(2.0),
            Some(Instant::now()),
            Some((0.25, 1.25)),
            true,
            None,
            Some(Duration::from_millis(10)),
        );
        player.play_span_frames = Some((12_345, 67_891));
        player.track_total_frames = Some(96_001);

        assert_eq!(player.playback_span_frames(), Some((12_345, 67_891)));
        assert_eq!(player.track_total_frames(), Some(96_001));
    }

    #[test]
    fn playback_elapsed_tracks_started_playback_state() {
        let Ok(outcome) = open_output_stream(&AudioOutputConfig::default()) else {
            return;
        };
        let stream = outcome.stream;
        let elapsed = duration_for_frames(1_200, 48_000);
        let player = AudioPlayer::test_with_state(
            stream,
            Some(1.0),
            Some(Instant::now()),
            Some((0.0, 1.0)),
            false,
            None,
            Some(elapsed),
        );

        assert_eq!(player.playback_elapsed(), Some(elapsed));
    }

    #[test]
    fn progress_applies_loop_offset_in_frames() {
        let Ok(outcome) = open_output_stream(&AudioOutputConfig::default()) else {
            return;
        };
        let stream = outcome.stream;
        let elapsed = duration_for_frames(950, 48_000);
        let mut player = AudioPlayer::test_with_state(
            stream,
            Some(2.0),
            Some(Instant::now()),
            Some((0.0, 2.0)),
            true,
            Some(0.0),
            Some(elapsed),
        );
        player.sample_rate = Some(48_000);
        player.track_total_frames = Some(5_000);
        player.play_span_frames = Some((1_000, 2_000));
        player.loop_offset_frames = Some(100);

        let progress = player.progress().expect("progress");
        let elapsed_frames = duration_to_frames_floor(elapsed, 48_000);
        let expected_frame = 1_000 + ((100 + elapsed_frames) % 1_000);
        let expected = expected_frame as f32 / 5_000.0;
        assert!((progress - expected).abs() < 1e-6);
    }

    #[test]
    fn progress_applies_one_shot_retarget_offset_in_frames() {
        let Ok(outcome) = open_output_stream(&AudioOutputConfig::default()) else {
            return;
        };
        let stream = outcome.stream;
        let elapsed = duration_for_frames(950, 48_000);
        let mut player = AudioPlayer::test_with_state(
            stream,
            Some(2.0),
            Some(Instant::now()),
            Some((0.0, 2.0)),
            false,
            Some(0.0),
            Some(elapsed),
        );
        player.sample_rate = Some(48_000);
        player.track_total_frames = Some(5_000);
        player.play_span_frames = Some((1_000, 2_000));
        player.loop_offset_frames = Some(100);

        let progress = player.progress().expect("progress");
        let elapsed_frames = duration_to_frames_floor(elapsed, 48_000);
        let expected_frame = 1_000 + (100 + elapsed_frames).min(1_000);
        let expected = expected_frame as f32 / 5_000.0;
        assert!((progress - expected).abs() < 1e-6);
    }
}
