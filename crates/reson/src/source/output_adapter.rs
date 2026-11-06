//! Output-format adapter for playback sources.

use super::Source;
use super::streaming_resampler::StreamingResampler;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

/// Identifies an interleaved stereo output pair by its zero-based pair index.
///
/// This is deliberately a channel-index contract. It does not infer a
/// speaker layout from the output channel count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StereoOutputPair(usize);

impl StereoOutputPair {
    #[cfg(test)]
    pub(crate) const fn new(pair_index: usize) -> Self {
        Self(pair_index)
    }

    fn for_output_channels(self, output_channels: usize) -> Self {
        let pair_count = output_channels / 2;
        if self.0 < pair_count {
            self
        } else {
            Self::default()
        }
    }

    fn first_channel(self) -> usize {
        self.0 * 2
    }

    pub(crate) fn can_bypass(self, source_channels: usize, target_channels: usize) -> bool {
        self == Self::default()
            && ((source_channels == 1 && target_channels == 1)
                || (source_channels == 2 && target_channels == 2))
    }
}

/// Adapt one source to a concrete output sample rate and channel count.
///
/// The playback callback mixes one interleaved sample per device-buffer slot, so
/// queued sources must already match the opened output stream format. This
/// adapter remaps channels and uses linear resampling when the rates differ.
pub(crate) struct OutputAdapter<S> {
    inner: S,
    source_channels: usize,
    target_channels: usize,
    output_pair: StereoOutputPair,
    source_rate: u32,
    target_rate: u32,
    current_frame: Vec<f32>,
    next_frame: Vec<f32>,
    current_frame_valid: bool,
    next_frame_valid: bool,
    base_frame_index: u64,
    position_num: u64,
    pending_output: Vec<f32>,
    pending_index: usize,
}

impl<S> OutputAdapter<S>
where
    S: Source,
{
    /// Build an adapter that emits samples in the requested output format.
    #[cfg(test)]
    pub(crate) fn new(inner: S, target_rate: u32, target_channels: u16) -> Self {
        Self::with_output_pair(
            inner,
            target_rate,
            target_channels,
            StereoOutputPair::default(),
        )
    }

    /// Build an adapter that places a stereo audition program on one output
    /// pair, silencing every unrelated output channel.
    pub(crate) fn with_output_pair(
        mut inner: S,
        target_rate: u32,
        target_channels: u16,
        output_pair: StereoOutputPair,
    ) -> Self {
        let source_channels = inner.channels().max(1) as usize;
        let source_rate = inner.sample_rate().max(1);
        let target_rate = target_rate.max(1);
        let mut current_frame = vec![0.0; source_channels];
        let (current_frame_valid, next_frame, next_frame_valid) = if source_rate == target_rate {
            // Routing alone needs one frame at a time and no interpolator history.
            (true, Vec::new(), false)
        } else {
            let current_valid = read_frame_into(&mut inner, &mut current_frame);
            let mut next_frame = vec![0.0; source_channels];
            let next_valid = read_frame_into(&mut inner, &mut next_frame);
            (current_valid, next_frame, next_valid)
        };
        let target_channels = target_channels.max(1) as usize;
        let output_pair = output_pair.for_output_channels(target_channels);
        Self {
            source_rate,
            target_rate,
            target_channels,
            output_pair,
            inner,
            source_channels,
            current_frame,
            next_frame,
            current_frame_valid,
            next_frame_valid,
            base_frame_index: 0,
            position_num: 0,
            pending_output: vec![0.0; target_channels],
            pending_index: target_channels,
        }
    }

    fn fill_pending_output(&mut self) -> Option<()> {
        let fraction = if self.source_rate == self.target_rate {
            self.current_frame_valid = self.current_frame_valid
                && read_frame_into(&mut self.inner, &mut self.current_frame);
            0.0
        } else {
            self.align_to_position()?;
            let fraction =
                (self.position_num % u64::from(self.target_rate)) as f32 / self.target_rate as f32;
            self.position_num = self
                .position_num
                .saturating_add(u64::from(self.source_rate));
            fraction
        };
        if !self.current_frame_valid {
            return None;
        }
        let current = self.current_frame.as_slice();
        let next = self.next_frame_valid.then_some(self.next_frame.as_slice());

        if self.target_channels == 1 && self.source_channels > 1 {
            let total = (0..self.source_channels)
                .map(|channel| interpolate_channel(current, next, channel, fraction))
                .sum::<f32>();
            self.pending_output[0] = total / self.source_channels as f32;
        } else {
            let first_channel = self.output_pair.first_channel();
            for output_channel in 0..self.target_channels {
                let pair_offset = output_channel.checked_sub(first_channel);
                self.pending_output[output_channel] = match pair_offset {
                    Some(0) => interpolate_channel(current, next, 0, fraction),
                    Some(1) => interpolate_channel(
                        current,
                        next,
                        if self.source_channels > 1 { 1 } else { 0 },
                        fraction,
                    ),
                    _ => 0.0,
                };
            }
        }

        self.pending_index = 0;
        Some(())
    }

    fn align_to_position(&mut self) -> Option<()> {
        loop {
            let requested_frame = self.position_num / self.target_rate as u64;
            match self.next_frame_valid {
                true if requested_frame > self.base_frame_index => self.advance_frame()?,
                false if requested_frame > self.base_frame_index => return None,
                _ => return Some(()),
            }
        }
    }

    fn advance_frame(&mut self) -> Option<()> {
        std::mem::swap(&mut self.current_frame, &mut self.next_frame);
        self.current_frame_valid = self.next_frame_valid;
        self.base_frame_index = self.base_frame_index.saturating_add(1);
        self.next_frame_valid = read_frame_into(&mut self.inner, &mut self.next_frame);
        self.current_frame_valid.then_some(())
    }
}

impl<S> Iterator for OutputAdapter<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pending_index >= self.pending_output.len() {
            self.fill_pending_output()?;
        }
        let sample = *self.pending_output.get(self.pending_index)?;
        self.pending_index = self.pending_index.saturating_add(1);
        Some(sample)
    }
}

impl<S> Source for OutputAdapter<S>
where
    S: Source,
{
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.target_channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.target_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

/// Route a filtered stereo program to the requested device channels.
pub(crate) struct BandLimitedOutputAdapter<S> {
    stream: StreamingResampler<ProgramFrames<S>>,
    position: RenderedSourcePosition,
    target_rate: u32,
    target_channels: usize,
    output_pair: StereoOutputPair,
    pending: [f32; 2],
    pending_index: usize,
}

/// Complete device frames rendered by one resampled source, in source frames.
#[derive(Clone, Default)]
pub(crate) struct RenderedSourcePosition(Arc<AtomicU64>);

impl RenderedSourcePosition {
    pub(crate) fn source_frame(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl<S: Source> BandLimitedOutputAdapter<S> {
    pub(crate) fn new(
        source: S,
        target_rate: u32,
        target_channels: u16,
        output_pair: StereoOutputPair,
    ) -> Option<Self> {
        let target_rate = target_rate.max(1);
        let target_channels = usize::from(target_channels.max(1));
        let source_rate = source.sample_rate();
        let frames = ProgramFrames {
            frame: vec![0.0; usize::from(source.channels().max(1))],
            source,
            fold_to_mono: target_channels == 1,
        };
        Some(Self {
            stream: StreamingResampler::new(frames, source_rate, target_rate)?,
            position: RenderedSourcePosition::default(),
            target_rate,
            target_channels,
            output_pair: output_pair.for_output_channels(target_channels),
            pending: [0.0; 2],
            pending_index: target_channels,
        })
    }

    pub(crate) fn rendered_position(&self) -> RenderedSourcePosition {
        self.position.clone()
    }
}

impl<S: Source> Iterator for BandLimitedOutputAdapter<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pending_index == self.target_channels {
            self.pending = self.stream.next()?;
            self.pending_index = 0;
        }
        let channel = self.pending_index;
        self.pending_index += 1;
        if self.pending_index == self.target_channels {
            self.position
                .0
                .store(self.stream.next_source_position().0, Ordering::Relaxed);
        }
        Some(
            channel
                .checked_sub(self.output_pair.first_channel())
                .and_then(|offset| self.pending.get(offset))
                .copied()
                .unwrap_or(0.0),
        )
    }
}

impl<S: Source> Source for BandLimitedOutputAdapter<S> {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.target_channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.target_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.stream.input().source.total_duration()
    }

    fn last_error(&self) -> Option<String> {
        self.stream.input().source.last_error()
    }
}

struct ProgramFrames<S> {
    source: S,
    frame: Vec<f32>,
    fold_to_mono: bool,
}

impl<S: Source> Iterator for ProgramFrames<S> {
    type Item = [f32; 2];

    fn next(&mut self) -> Option<Self::Item> {
        if !read_frame_into(&mut self.source, &mut self.frame) {
            return None;
        }
        let left = self.frame[0];
        if self.fold_to_mono {
            Some([
                self.frame.iter().sum::<f32>() / self.frame.len() as f32,
                0.0,
            ])
        } else {
            Some([left, self.frame.get(1).copied().unwrap_or(left)])
        }
    }
}

fn read_frame_into<S>(source: &mut S, frame: &mut [f32]) -> bool
where
    S: Source,
{
    for sample in frame {
        let Some(next) = source.next() else {
            return false;
        };
        *sample = next;
    }
    true
}

fn interpolate_channel(
    current: &[f32],
    next: Option<&[f32]>,
    channel: usize,
    fraction: f32,
) -> f32 {
    let start = current.get(channel).copied().unwrap_or(0.0);
    let Some(next) = next else {
        return start;
    };
    let end = next.get(channel).copied().unwrap_or(start);
    start + (end - start) * fraction
}

#[cfg(test)]
mod tests {
    use super::BandLimitedOutputAdapter;
    use super::OutputAdapter;
    use super::StereoOutputPair;
    use crate::Source;
    use std::time::Duration;

    struct FrameSource {
        channels: u16,
        sample_rate: u32,
        samples: Vec<f32>,
        index: usize,
    }

    impl FrameSource {
        fn new(channels: u16, sample_rate: u32, samples: Vec<f32>) -> Self {
            Self {
                channels,
                sample_rate,
                samples,
                index: 0,
            }
        }
    }

    impl Iterator for FrameSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = *self.samples.get(self.index)?;
            self.index += 1;
            Some(sample)
        }
    }

    impl Source for FrameSource {
        fn current_frame_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.index))
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn adapter_upsamples_and_duplicates_mono_frames() {
        let source = FrameSource::new(1, 2, vec![0.0, 1.0]);
        let adapted = OutputAdapter::new(source, 4, 2);
        let samples: Vec<f32> = adapted.collect();
        let expected = [0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0];
        assert_eq!(samples.len(), expected.len());
        for (got, exp) in samples.iter().zip(expected) {
            assert!((got - exp).abs() < 1e-6, "got {got} expected {exp}");
        }
    }

    #[test]
    fn adapter_downsamples_and_folds_stereo_to_mono() {
        let source = FrameSource::new(2, 4, vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0]);
        let adapted = OutputAdapter::new(source, 2, 1);
        let samples: Vec<f32> = adapted.collect();
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 1.0).abs() < 1e-6);
        assert!((samples[1] - 9.0).abs() < 1e-6);
    }

    #[test]
    fn stereo_and_mono_route_only_to_selected_pair_for_supported_output_formats() {
        for target_channels in [2, 4, 6, 8] {
            let pair_count = usize::from(target_channels / 2);
            for pair_index in 0..pair_count {
                let first_channel = pair_index * 2;

                let source = FrameSource::new(2, 48_000, vec![0.25, -0.5]);
                let samples: Vec<f32> = OutputAdapter::with_output_pair(
                    source,
                    48_000,
                    target_channels,
                    StereoOutputPair::new(pair_index),
                )
                .collect();
                let mut expected = vec![0.0; target_channels as usize];
                expected[first_channel] = 0.25;
                expected[first_channel + 1] = -0.5;
                assert_eq!(
                    samples, expected,
                    "stereo: {target_channels}ch pair {pair_index}"
                );

                let source = FrameSource::new(1, 48_000, vec![0.75]);
                let samples: Vec<f32> = OutputAdapter::with_output_pair(
                    source,
                    48_000,
                    target_channels,
                    StereoOutputPair::new(pair_index),
                )
                .collect();
                let mut expected = vec![0.0; target_channels as usize];
                expected[first_channel] = 0.75;
                expected[first_channel + 1] = 0.75;
                assert_eq!(
                    samples, expected,
                    "mono: {target_channels}ch pair {pair_index}"
                );
            }
        }
    }

    #[test]
    fn multichannel_source_uses_explicit_first_stereo_pair_without_layout_inference() {
        let source = FrameSource::new(6, 48_000, vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        let samples: Vec<f32> = OutputAdapter::new(source, 48_000, 8).collect();
        assert_eq!(samples, [0.1, 0.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn equal_rate_routing_preserves_pcm_bits_and_reads_only_rendered_frames() {
        let source = FrameSource::new(2, 48_000, vec![-0.0, f32::MAX, 0.0, -f32::MAX, 0.5]);
        let mut adapted =
            OutputAdapter::with_output_pair(source, 48_000, 4, StereoOutputPair::new(1));
        assert_eq!(adapted.inner.index, 0);

        let allocation_counts =
            crate::test_support::count_thread_allocations_and_deallocations(|| {
                for (index, (left, right)) in [(-0.0_f32, f32::MAX), (0.0, -f32::MAX)]
                    .into_iter()
                    .enumerate()
                {
                    for expected in [0.0_f32, 0.0, left, right] {
                        assert_eq!(adapted.next().map(f32::to_bits), Some(expected.to_bits()));
                        assert_eq!(adapted.inner.index, (index + 1) * 2);
                    }
                }
                assert_eq!(adapted.next(), None);
                assert_eq!(adapted.next(), None);
                assert_eq!(adapted.inner.index, 5);
            });
        assert_eq!(allocation_counts, (0, 0));
    }

    #[test]
    fn stale_pair_falls_back_to_pair_zero_after_output_format_rebuild() {
        let selected = StereoOutputPair::new(2);
        assert_eq!(selected.for_output_channels(8), selected);
        assert!(!selected.can_bypass(2, 8));
        assert_eq!(selected.for_output_channels(2), StereoOutputPair::default());
        assert!(!selected.can_bypass(2, 2));
        assert!(StereoOutputPair::default().can_bypass(2, 2));

        let source = FrameSource::new(2, 48_000, vec![0.1, 0.2]);
        let samples: Vec<f32> =
            OutputAdapter::with_output_pair(source, 48_000, 2, selected).collect();
        assert_eq!(samples, [0.1, 0.2]);
    }

    #[test]
    fn sustained_44100_to_48000_adaptation_allocates_nothing_after_construction() {
        let frames = 4_410;
        let samples = (0..frames * 2)
            .map(|sample| sample as f32 / (frames * 2) as f32)
            .collect();
        let source = FrameSource::new(2, 44_100, samples);
        let mut adapted = OutputAdapter::new(source, 48_000, 2);
        let mut emitted = 0usize;

        let allocations = crate::test_support::count_thread_allocations(|| {
            while adapted.next().is_some() {
                emitted += 1;
            }
        });

        assert!(emitted > frames * 2);
        assert_eq!(allocations, 0);
    }

    #[test]
    fn band_limited_position_uses_complete_device_frames_and_is_source_owned() {
        let source = || FrameSource::new(2, 44_100, vec![0.5; 882]);
        let mut current =
            BandLimitedOutputAdapter::new(source(), 48_000, 4, StereoOutputPair::default())
                .unwrap();
        let position = current.rendered_position();
        let replacement =
            BandLimitedOutputAdapter::new(source(), 48_000, 4, StereoOutputPair::default())
                .unwrap();
        let replacement_position = replacement.rendered_position();

        let counts = crate::test_support::count_thread_allocations_and_deallocations(|| {
            assert_eq!(position.source_frame(), 0);
            assert!(current.next().is_some());
            assert_eq!(current.stream.input().source.index, 162);
            assert_eq!(position.source_frame(), 0);
            for channel in 1..8 {
                assert!(current.next().is_some());
                assert_eq!(position.source_frame(), u64::from(channel == 7));
            }
            for _ in 2..160 {
                for _ in 0..4 {
                    assert!(current.next().is_some());
                }
            }
            assert_eq!(position.source_frame(), 147);
            assert_eq!(replacement_position.source_frame(), 0);
        });
        assert_eq!(counts, (0, 0));
    }
}
