//! Allocation-free streaming state for prepared band-limited rate conversion.

use super::resample_kernel::ResampleKernel;

// Covers even 384 kHz to 8 kHz and bounds input work per rendered frame.
const MAX_DOWNSAMPLE_RATIO: u64 = 64;

pub(crate) fn supports_band_limited_rate_conversion(source_rate: u32, target_rate: u32) -> bool {
    u64::from(source_rate.max(1)) <= MAX_DOWNSAMPLE_RATIO * u64::from(target_rate.max(1))
}

pub(crate) struct StreamingResampler<I> {
    input: I,
    kernel: ResampleKernel,
    source_rate: u32,
    target_rate: u32,
    next_source_frame: u64,
    phase: u32,
    history: Box<[[f32; 2]]>,
    window_start: usize,
    window_end: u64,
    input_frames_read: u64,
    last: [f32; 2],
    input_ended: bool,
}

impl<I> StreamingResampler<I>
where
    I: Iterator<Item = [f32; 2]>,
{
    /// Prepare fixed storage without reading the source. Rates that require
    /// advancing more than 64 input frames per output frame are declined.
    pub(crate) fn new(input: I, source_rate: u32, target_rate: u32) -> Option<Self> {
        let source_rate = source_rate.max(1);
        let target_rate = target_rate.max(1);
        if !supports_band_limited_rate_conversion(source_rate, target_rate) {
            return None;
        }
        let kernel = ResampleKernel::new(source_rate, target_rate);
        let history = vec![[0.0; 2]; 4 * kernel.radius()].into_boxed_slice();
        Some(Self {
            input,
            kernel,
            source_rate,
            target_rate,
            next_source_frame: 0,
            phase: 0,
            history,
            window_start: 0,
            window_end: 0,
            input_frames_read: 0,
            last: [0.0; 2],
            input_ended: false,
        })
    }

    /// Exact source coordinate of the next output frame as an integer frame
    /// and the fractional numerator over the target rate. The read head can
    /// be ahead of this coordinate by the kernel's future support.
    pub(crate) fn next_source_position(&self) -> (u64, u32) {
        (self.next_source_frame, self.phase)
    }

    pub(crate) fn input(&self) -> &I {
        &self.input
    }

    fn push_history_frame(&mut self, frame: [f32; 2]) {
        let taps = self.history.len() / 2;
        // Duplicate the ring so even a window that wraps is a contiguous slice.
        self.history[self.window_start] = frame;
        self.history[self.window_start + taps] = frame;
        self.window_start += 1;
        if self.window_start == taps {
            self.window_start = 0;
        }
        self.window_end += 1;
    }

    fn prepare_window(&mut self) -> Option<()> {
        if self.input_ended && self.next_source_frame >= self.input_frames_read {
            return None;
        }
        if self.window_end == 0 {
            let Some(first) = self.input.next() else {
                self.input_ended = true;
                return None;
            };
            self.history.fill(first);
            self.last = first;
            self.input_frames_read = 1;
            self.window_end = 1;
        }

        let window_end = self
            .next_source_frame
            .saturating_add(self.kernel.radius() as u64 + 1);
        while self.window_end < window_end {
            if !self.input_ended {
                if let Some(frame) = self.input.next() {
                    self.last = frame;
                    self.input_frames_read += 1;
                } else {
                    self.input_ended = true;
                    if self.next_source_frame >= self.input_frames_read {
                        return None;
                    }
                }
            }
            // Pad with the last frame only while the output center is in range.
            self.push_history_frame(self.last);
        }
        Some(())
    }
    /// Render only while the center is inside an exclusive source endpoint.
    /// Keep read-ahead intact so the caller can extend the endpoint before it
    /// retires the source. Each call observes one endpoint for the whole frame.
    pub(crate) fn next_before(&mut self, source_end: u64) -> Option<[f32; 2]> {
        let (source_frame, phase) = self.next_source_position();
        if source_frame >= source_end {
            return None;
        }
        self.prepare_window()?;
        let frames = &self.history[self.window_start..self.window_start + self.history.len() / 2];
        let future_frames = source_end - source_frame - 1;
        let radius = self.kernel.radius();
        let result = if future_frames < radius as u64 {
            self.kernel
                .render_at_right_edge(frames, radius + future_frames as usize, phase)
        } else {
            self.kernel.render(frames, phase)
        };
        let next_phase = u64::from(phase) + u64::from(self.source_rate);
        let target_rate = u64::from(self.target_rate);
        self.next_source_frame = source_frame.saturating_add(next_phase / target_rate);
        self.phase = (next_phase % target_rate) as u32;
        Some(result)
    }
}

impl<I> Iterator for StreamingResampler<I>
where
    I: Iterator<Item = [f32; 2]>,
{
    type Item = [f32; 2];

    fn next(&mut self) -> Option<Self::Item> {
        self.next_before(u64::MAX)
    }
}
