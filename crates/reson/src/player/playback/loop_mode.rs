use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use crate::Source;

const LOOP_FOREVER: u64 = u64::MAX;
const FINISHED: u64 = u64::MAX - 1;

/// Control shared by the runtime and the source consumed by the audio callback.
/// The decoder may run ahead, so the cursor is published by this outer source.
#[derive(Clone)]
pub(in crate::player) struct LoopModeHandle {
    state: Arc<LoopModeState>,
}

struct LoopModeState {
    stop_after_frames: AtomicU64,
    frames_consumed: AtomicU64,
    frame_count: u64,
    initial_offset: u64,
}

impl LoopModeHandle {
    pub(super) fn new(looped: bool, frame_count: u64, initial_offset: u64) -> Self {
        let frame_count = frame_count.max(1);
        let initial_offset = initial_offset.min(frame_count - 1);
        Self {
            state: Arc::new(LoopModeState {
                stop_after_frames: AtomicU64::new(if looped {
                    LOOP_FOREVER
                } else {
                    frame_count - initial_offset
                }),
                frames_consumed: AtomicU64::new(0),
                frame_count,
                initial_offset,
            }),
        }
    }

    /// Returns false if the audio source has already completed and must be restarted.
    pub(super) fn set_looped(&self, looped: bool) -> bool {
        let mut current = self.state.stop_after_frames.load(Ordering::Acquire);
        loop {
            if current == FINISHED {
                return false;
            }
            let stop_after = if looped {
                LOOP_FOREVER
            } else {
                let frames = self.frames_consumed();
                let position =
                    self.state.initial_offset.saturating_add(frames) % self.state.frame_count;
                frames
                    .saturating_add(self.state.frame_count - position)
                    .min(FINISHED - 1)
            };
            match self.state.stop_after_frames.compare_exchange(
                current,
                stop_after,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(latest) => current = latest,
            }
        }
    }

    pub(super) fn looped(&self) -> bool {
        self.state.stop_after_frames.load(Ordering::Acquire) == LOOP_FOREVER
    }

    #[cfg(test)]
    pub(super) fn finished(&self) -> bool {
        self.state.stop_after_frames.load(Ordering::Acquire) == FINISHED
    }

    pub(super) fn mark_finished(&self) {
        self.state
            .stop_after_frames
            .store(FINISHED, Ordering::Release);
    }

    pub(in crate::player) fn frames_consumed(&self) -> u64 {
        self.state.frames_consumed.load(Ordering::Acquire)
    }

    pub(in crate::player) fn progress_offset(&self) -> u64 {
        let frames = self.frames_consumed();
        let stop_after = self.state.stop_after_frames.load(Ordering::Acquire);
        if stop_after == FINISHED || frames >= stop_after {
            self.state.frame_count
        } else {
            self.state.initial_offset.saturating_add(frames) % self.state.frame_count
        }
    }
}

/// Turns a repeating source into a one-shot at the end of its current cycle.
/// This sits outside AsyncSource so a mode update takes effect at playback,
/// rather than at the decoder's prefetched position.
pub(super) struct LiveLoopModeSource<S> {
    inner: S,
    handle: LoopModeHandle,
    channels: u16,
    channel_position: u16,
}

impl<S: Source> LiveLoopModeSource<S> {
    pub(super) fn new(inner: S, handle: LoopModeHandle) -> Self {
        Self {
            channels: inner.channels().max(1),
            inner,
            handle,
            channel_position: 0,
        }
    }
}

impl<S: Source<Item = f32>> Iterator for LiveLoopModeSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.channel_position == 0 {
            let frames = self.handle.frames_consumed();
            let stop_after = self.handle.state.stop_after_frames.load(Ordering::Acquire);
            if stop_after == FINISHED {
                return None;
            }
            if frames >= stop_after {
                match self.handle.state.stop_after_frames.compare_exchange(
                    stop_after,
                    FINISHED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) | Err(FINISHED) => return None,
                    Err(_) => {} // A newer mode change won the boundary race.
                }
            }
        }
        let sample = match self.inner.next() {
            Some(sample) => sample,
            None => {
                self.handle.mark_finished();
                return None;
            }
        };
        self.channel_position += 1;
        if self.channel_position == self.channels {
            self.channel_position = 0;
            self.handle
                .state
                .frames_consumed
                .fetch_add(1, Ordering::Release);
        }
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AsyncSource, SamplesBuffer};

    #[test]
    fn disabling_loop_finishes_current_cycle_without_restarting_audio() {
        let handle = LoopModeHandle::new(true, 4, 0);
        let source = SamplesBuffer::new(1, 1_000, vec![1.0, 2.0, 3.0, 4.0]).repeat_infinite();
        let mut source = LiveLoopModeSource::new(source, handle.clone());
        assert_eq!(
            (0..6).map(|_| source.next().unwrap()).collect::<Vec<_>>(),
            vec![1.0, 2.0, 3.0, 4.0, 1.0, 2.0]
        );
        handle.set_looped(false);
        assert_eq!(source.by_ref().collect::<Vec<_>>(), vec![3.0, 4.0]);
        assert!(handle.finished());
        assert_eq!(handle.progress_offset(), 4);
    }

    #[test]
    fn enabling_loop_keeps_one_shot_alive_at_boundary() {
        let handle = LoopModeHandle::new(false, 4, 0);
        let source = SamplesBuffer::new(1, 1_000, vec![1.0, 2.0, 3.0, 4.0]).repeat_infinite();
        let mut source = LiveLoopModeSource::new(source, handle.clone());
        assert_eq!(source.next(), Some(1.0));
        assert_eq!(source.next(), Some(2.0));
        handle.set_looped(true);
        assert_eq!(
            (0..4).map(|_| source.next().unwrap()).collect::<Vec<_>>(),
            vec![3.0, 4.0, 1.0, 2.0]
        );
        assert!(!handle.finished());
        assert_eq!(handle.progress_offset(), 2);
    }

    #[test]
    fn stereo_mode_change_waits_for_whole_frame() {
        let handle = LoopModeHandle::new(false, 2, 0);
        let source = SamplesBuffer::new(2, 1_000, vec![1.0, 2.0, 3.0, 4.0]).repeat_infinite();
        let mut source = LiveLoopModeSource::new(source, handle.clone());
        assert_eq!(source.next(), Some(1.0));
        assert_eq!(handle.frames_consumed(), 0);
        handle.set_looped(true);
        assert_eq!(source.next(), Some(2.0));
        assert_eq!(handle.frames_consumed(), 1);
        assert_eq!(source.next(), Some(3.0));
    }

    #[test]
    fn prefetched_decoder_audio_does_not_delay_loop_off() {
        let handle = LoopModeHandle::new(true, 4, 0);
        let decoded = SamplesBuffer::new(1, 1_000, vec![1.0, 2.0, 3.0, 4.0]).repeat_infinite();
        let mut async_source = AsyncSource::with_buffer_seconds(decoded, 1.0);
        async_source.prefill_for_duration(Duration::from_millis(10), Duration::from_millis(100));
        let mut source = LiveLoopModeSource::new(async_source, handle.clone());

        assert_eq!(source.next(), Some(1.0));
        assert_eq!(source.next(), Some(2.0));
        handle.set_looped(false);
        assert_eq!(source.by_ref().collect::<Vec<_>>(), vec![3.0, 4.0]);
        assert!(handle.finished());
    }

    #[test]
    fn completed_one_shot_cannot_be_revived_by_late_mode_change() {
        let handle = LoopModeHandle::new(false, 2, 0);
        let source = SamplesBuffer::new(1, 1_000, vec![1.0, 2.0]).repeat_infinite();
        let mut source = LiveLoopModeSource::new(source, handle.clone());
        assert_eq!(source.next(), Some(1.0));
        assert_eq!(source.next(), Some(2.0));
        assert_eq!(source.next(), None);
        assert!(!handle.set_looped(true));
        assert_eq!(source.next(), None);
    }
}

impl<S: Source<Item = f32>> Source for LiveLoopModeSource<S> {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}
