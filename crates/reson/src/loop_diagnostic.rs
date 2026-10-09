use crate::Source;
use std::sync::OnceLock;

static LOOP_CYCLE_DIAGNOSTICS_ENABLED: OnceLock<bool> = OnceLock::new();

/// Diagnostic wrapper that summarizes completed loop cycles when disposed.
/// Cycle accounting is kept in `next`; formatting and logging happen only in
/// `Drop`, after the native callback has relinquished the source.
pub(crate) struct LoopDiagnostic<S> {
    inner: S,
    samples_emitted: u64,
    completed_cycles: u64,
    expected_samples_per_cycle: u64,
    channels: u16,
    diagnostics_enabled: bool,
}

impl<S> LoopDiagnostic<S> {
    pub(crate) fn new(inner: S, expected_samples_per_cycle: u64) -> Self
    where
        S: Source<Item = f32>,
    {
        let channels = inner.channels();
        Self {
            inner,
            samples_emitted: 0,
            completed_cycles: 0,
            expected_samples_per_cycle,
            channels,
            diagnostics_enabled: loop_cycle_diagnostics_enabled(),
        }
    }
}

impl<S> Iterator for LoopDiagnostic<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        self.samples_emitted += 1;

        // Check if we've completed a cycle
        if self.samples_emitted >= self.expected_samples_per_cycle {
            self.completed_cycles = self.completed_cycles.saturating_add(1);
            self.samples_emitted = 0;
        }

        Some(sample)
    }
}

impl<S> Source for LoopDiagnostic<S>
where
    S: Source,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None // Infinite loop
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

impl<S> Drop for LoopDiagnostic<S> {
    fn drop(&mut self) {
        if self.diagnostics_enabled {
            tracing::debug!(
                target: "audio::loop_diagnostic",
                completed_cycles = self.completed_cycles,
                trailing_samples = self.samples_emitted,
                expected_samples_per_cycle = self.expected_samples_per_cycle,
                channels = self.channels,
                "Loop playback diagnostic summary"
            );
        }
    }
}

fn loop_cycle_diagnostics_enabled() -> bool {
    *LOOP_CYCLE_DIAGNOSTICS_ENABLED.get_or_init(|| {
        std::env::var("RESON_LOOP_CYCLE_DIAGNOSTICS")
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::loop_cycle_diagnostics_enabled;

    #[test]
    fn loop_cycle_diagnostics_default_to_disabled() {
        assert!(!loop_cycle_diagnostics_enabled());
    }
}
