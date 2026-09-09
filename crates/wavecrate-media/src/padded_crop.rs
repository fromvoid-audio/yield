//! In-place signed crops using capacity reserved before source decoding.

#[derive(Clone, Copy, Debug)]
pub(super) struct PaddedCropPlan {
    pub(super) output_samples: usize,
    source_samples: usize,
    source_start: usize,
    source_end: usize,
    target_start: usize,
}

impl PaddedCropPlan {
    pub(super) fn new(
        total_frames: usize,
        channels: usize,
        start_frame: i64,
        end_frame: i64,
    ) -> Result<Self, String> {
        let invalid = || String::from("invalid or oversized padded crop bounds");
        if channels == 0 || total_frames == 0 {
            return Err(invalid());
        }
        let total = i64::try_from(total_frames).map_err(|_| invalid())?;
        let frames = end_frame
            .checked_sub(start_frame)
            .filter(|frames| *frames > 0)
            .and_then(|frames| usize::try_from(frames).ok())
            .ok_or_else(invalid)?;
        let output_samples = frames.checked_mul(channels).ok_or_else(invalid)?;
        let source_samples = total_frames.checked_mul(channels).ok_or_else(invalid)?;
        let start = start_frame.clamp(0, total);
        let end = end_frame.clamp(0, total);
        let target_frame = if end > start {
            usize::try_from(start.checked_sub(start_frame).ok_or_else(invalid)?)
                .map_err(|_| invalid())?
        } else {
            0
        };
        Ok(Self {
            output_samples,
            source_samples,
            source_start: usize::try_from(start)
                .map_err(|_| invalid())?
                .checked_mul(channels)
                .ok_or_else(invalid)?,
            source_end: usize::try_from(end)
                .map_err(|_| invalid())?
                .checked_mul(channels)
                .ok_or_else(invalid)?,
            target_start: target_frame.checked_mul(channels).ok_or_else(invalid)?,
        })
    }

    pub(super) fn apply(
        &self,
        samples: &mut Vec<f32>,
        mut check: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        check()?;
        if samples.len() != self.source_samples || samples.capacity() < self.output_samples {
            return Err(String::from(
                "padded crop buffer was not reserved for this plan",
            ));
        }
        while samples.len() < self.output_samples {
            check()?;
            let next = samples.len() + (self.output_samples - samples.len()).min(4096);
            samples.resize(next, 0.0);
        }
        let count = self.source_end - self.source_start;
        if self.target_start > self.source_start {
            let mut remaining = count;
            while remaining > 0 {
                check()?;
                let offset = remaining - remaining.min(4096);
                samples.copy_within(
                    self.source_start + offset..self.source_start + remaining,
                    self.target_start + offset,
                );
                remaining = offset;
            }
        } else if self.target_start < self.source_start {
            for offset in (0..count).step_by(4096) {
                check()?;
                let size = (count - offset).min(4096);
                samples.copy_within(
                    self.source_start + offset..self.source_start + offset + size,
                    self.target_start + offset,
                );
            }
        }
        check()?;
        samples.truncate(self.output_samples);
        for block in samples[..self.target_start].chunks_mut(4096) {
            check()?;
            block.fill(0.0);
        }
        for block in samples[self.target_start + count..].chunks_mut(4096) {
            check()?;
            block.fill(0.0);
        }
        check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(plan: PaddedCropPlan) -> Vec<f32> {
        let mut samples = Vec::with_capacity(plan.source_samples.max(plan.output_samples));
        samples.extend((1..=plan.source_samples).map(|value| value as f32));
        samples
    }

    #[test]
    fn overlapping_moves_preserve_audio_in_both_directions_without_reallocation() {
        for (start, end) in [(-1, 6001), (1, 5999)] {
            let plan = PaddedCropPlan::new(6000, 2, start, end).unwrap();
            let mut samples = buffer(plan);
            let original = samples.clone();
            let pointer = samples.as_ptr();
            plan.apply(&mut samples, || Ok(())).unwrap();
            assert_eq!(samples.as_ptr(), pointer);
            let expected: Vec<f32> = (start..end)
                .flat_map(|frame| {
                    if (0..6000).contains(&frame) {
                        original[frame as usize * 2..frame as usize * 2 + 2].to_vec()
                    } else {
                        vec![0.0; 2]
                    }
                })
                .collect();
            assert_eq!(samples, expected);
        }
    }

    #[test]
    fn cancellation_is_observed_during_growth_movement_and_zero_fill() {
        // Exercise every callback boundary, including partially initialized and moved buffers.
        for (start, end) in [(-5000, 7000), (1, 5999), (7000, 19000), (-19000, -7000)] {
            let plan = PaddedCropPlan::new(6000, 2, start, end).unwrap();
            let mut checks = 0;
            plan.apply(&mut buffer(plan), || {
                checks += 1;
                Ok(())
            })
            .unwrap();
            for stop_at in 1..=checks {
                let mut visited = 0;
                let result = plan.apply(&mut buffer(plan), || {
                    visited += 1;
                    if visited == stop_at {
                        Err(String::from("canceled fixture"))
                    } else {
                        Ok(())
                    }
                });
                assert!(result.unwrap_err().contains("canceled"));
                assert_eq!(visited, stop_at);
            }
        }
    }
}
