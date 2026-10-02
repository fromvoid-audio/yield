//! Fixed acquisition-local observations of the actual feature worker projection boundary.
use crate::{FeatureAnalysisBlocker, FeatureAnalysisState};
use std::time::Duration;

/// Authority-free diagnostics; Ready is a validated feature observation, never overall source
/// readiness, new publication, UI acknowledgement or physical qualification. During a request,
/// entered totals can exceed terminal/drop totals. Saturated values are lower bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureAnalysisMetrics {
    /// Actual feature-worker entries; submission rejection and commands discarded before this
    /// function is invoked (including worker shutdown draining) are excluded.
    pub entered: u64,
    /// Closed/retired/missing source prevented source work before any terminal observation.
    pub dropped_before_work: u64,
    /// Actual terminal Ready observations, including already-ready retries.
    pub terminal_ready: u64,
    /// Actual terminal Blocked observations.
    pub terminal_blocked: u64,
    /// Blocked observations explicitly classified StaleSource (subset of terminal_blocked).
    pub terminal_stale: u64,
    /// Blocked observations explicitly classified Cancelled (subset of terminal_blocked).
    pub terminal_cancelled: u64,
    /// Terminal events enqueued after the existing current producer fence checks.
    pub terminal_enqueued: u64,
    /// Terminal observations rejected by the existing producer publication fences.
    pub terminal_projection_rejected: u64,
    /// Monotonic elapsed microseconds for completed/drop requests; not queue wait or an SLO.
    pub total_elapsed_us: u64,
    /// Largest completed/drop request elapsed observation.
    pub max_elapsed_us: u64,
    /// A counter or elapsed observation exceeded its representation.
    pub saturated: bool,
}

/// Copy only bounded classification before the event takes payload custody.
#[derive(Clone, Copy)]
pub(super) enum TerminalObservation {
    Ready,
    Blocked(FeatureAnalysisBlocker),
}
impl TerminalObservation {
    pub(super) fn from_state(state: &FeatureAnalysisState) -> Option<Self> {
        match state {
            FeatureAnalysisState::Ready { .. } => Some(Self::Ready),
            FeatureAnalysisState::Blocked { reason } => Some(Self::Blocked(*reason)),
            _ => None,
        }
    }
}
fn add(slot: &mut u64, amount: u64, saturated: &mut bool) {
    match slot.checked_add(amount) {
        Some(value) => *slot = value,
        None => {
            *slot = u64::MAX;
            *saturated = true;
        }
    }
}
impl FeatureAnalysisMetrics {
    pub(super) fn enter(&mut self) {
        add(&mut self.entered, 1, &mut self.saturated);
    }
    fn elapsed(&mut self, elapsed: Duration) {
        let us = u64::try_from(elapsed.as_micros()).unwrap_or_else(|_| {
            self.saturated = true;
            u64::MAX
        });
        add(&mut self.total_elapsed_us, us, &mut self.saturated);
        self.max_elapsed_us = self.max_elapsed_us.max(us);
    }
    pub(super) fn drop_before_work(&mut self, elapsed: Duration) {
        add(&mut self.dropped_before_work, 1, &mut self.saturated);
        self.elapsed(elapsed);
    }
    pub(super) fn terminal(
        &mut self,
        observation: TerminalObservation,
        enqueued: bool,
        elapsed: Duration,
    ) {
        macro_rules! count {
            ($field:ident) => {
                add(&mut self.$field, 1, &mut self.saturated)
            };
        }
        match observation {
            TerminalObservation::Ready => count!(terminal_ready),
            TerminalObservation::Blocked(reason) => {
                count!(terminal_blocked);
                match reason {
                    FeatureAnalysisBlocker::StaleSource => count!(terminal_stale),
                    FeatureAnalysisBlocker::Cancelled => count!(terminal_cancelled),
                    _ => {}
                }
            }
        }
        if enqueued {
            count!(terminal_enqueued);
        } else {
            count!(terminal_projection_rejected);
        }
        self.elapsed(elapsed);
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;
    #[test]
    fn analysis_observer_scalar_saturation_and_representation_are_bounded() {
        // Representation only; actual worker/SQL/DSP projection cases live in catalog tests.
        let mut metrics = FeatureAnalysisMetrics {
            entered: u64::MAX,
            dropped_before_work: u64::MAX,
            terminal_ready: u64::MAX,
            terminal_projection_rejected: u64::MAX,
            total_elapsed_us: u64::MAX,
            ..Default::default()
        };
        metrics.enter();
        metrics.terminal(TerminalObservation::Ready, false, Duration::from_micros(4));
        metrics.drop_before_work(Duration::from_secs(u64::MAX));
        assert!(metrics.saturated);
        assert_eq!(metrics.entered, u64::MAX);
        assert_eq!(metrics.dropped_before_work, u64::MAX);
        assert_eq!(metrics.terminal_ready, u64::MAX);
        assert_eq!(metrics.terminal_projection_rejected, u64::MAX);
        assert_eq!(metrics.total_elapsed_us, u64::MAX);
        assert_eq!(metrics.max_elapsed_us, u64::MAX);
        assert!(std::mem::size_of::<FeatureAnalysisMetrics>() <= 96);
        assert!(format!("{metrics:?}").len() < 768);
    }
}
