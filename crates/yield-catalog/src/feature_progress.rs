//! Truthful bounded measurements and monotonic reduction of explicit feature requests.
use crate::FeatureAnalysisState;
use yield_library::sample_sources::SourceId;

/// Transient activity of one bounded feature request. This is not durable source completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FeatureAnalysisPhase {
    /// Inspect the current committed, completely audited manifest and its admission fences.
    InspectingManifest,
    /// Compare this exact target with validated stored feature bytes.
    ComparingReadiness,
    /// Queue unfinished work through the retained owner and prepare its claim.
    QueueingUnfinishedWork,
}
/// Measurement of transient preparation; no cumulative row visits or invented totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeaturePhaseMeasurement {
    /// Work has no published denominator; it is not a percentage or a completed-file count.
    Indeterminate,
    /// Exact readiness targets in this phase; never files, row visits or queued jobs.
    Targets {
        /// Targets checked so far.
        completed: u64,
        /// Frozen targets to check.
        total: u64,
    },
}
impl FeatureAnalysisPhase {
    /// Preparation does not run an extra manifest pass to obtain a denominator.
    pub fn measurement(self) -> FeaturePhaseMeasurement {
        match self {
            Self::InspectingManifest => FeaturePhaseMeasurement::Indeterminate,
            Self::ComparingReadiness | Self::QueueingUnfinishedWork => {
                FeaturePhaseMeasurement::Targets {
                    completed: 0,
                    total: 1,
                }
            }
        }
    }
    /// Plain activity text, separate from a durable readiness result.
    pub fn message(self) -> &'static str {
        match self {
            Self::InspectingManifest => "Inspecting audited source",
            Self::ComparingReadiness => "Checking feature readiness",
            Self::QueueingUnfinishedWork => "Preparing unfinished feature work",
        }
    }
}

impl FeatureAnalysisState {
    /// Measurement for this explicitly selected target, not aggregate source completion.
    pub fn measurement(&self) -> FeaturePhaseMeasurement {
        match self {
            Self::Preparing { phase } => phase.measurement(),
            Self::Queued | Self::Ready { .. } => FeaturePhaseMeasurement::Targets {
                completed: 1,
                total: 1,
            },
            Self::Running | Self::Blocked { .. } => FeaturePhaseMeasurement::Indeterminate,
        }
    }
    /// Combine one current request per distinct source (at most 64), in the same phase.
    /// The caller must first apply each source/lifecycle/revision/request fence. Mixed phases,
    /// unknown work, empty input or overflow never become an invented overall percentage.
    pub fn aggregate_measurement(states: &[(SourceId, Self)]) -> FeaturePhaseMeasurement {
        if states.is_empty() || states.len() > 64 {
            return FeaturePhaseMeasurement::Indeterminate;
        }
        let phase = states[0].1.progress_rank();
        let mut completed = 0u64;
        let mut total = 0u64;
        for (index, (source, state)) in states.iter().enumerate() {
            if source.as_str().is_empty()
                || states[..index]
                    .iter()
                    .any(|(previous, _)| previous == source)
            {
                return FeaturePhaseMeasurement::Indeterminate;
            }
            if state.progress_rank() != phase {
                return FeaturePhaseMeasurement::Indeterminate;
            }
            let FeaturePhaseMeasurement::Targets {
                completed: n,
                total: d,
            } = state.measurement()
            else {
                return FeaturePhaseMeasurement::Indeterminate;
            };
            let Some(next_n) = completed.checked_add(n) else {
                return FeaturePhaseMeasurement::Indeterminate;
            };
            let Some(next_d) = total.checked_add(d) else {
                return FeaturePhaseMeasurement::Indeterminate;
            };
            completed = next_n;
            total = next_d;
        }
        FeaturePhaseMeasurement::Targets { completed, total }
    }
    fn progress_rank(&self) -> u8 {
        match self {
            Self::Preparing {
                phase: FeatureAnalysisPhase::InspectingManifest,
            } => 0,
            Self::Preparing {
                phase: FeatureAnalysisPhase::ComparingReadiness,
            } => 1,
            Self::Preparing {
                phase: FeatureAnalysisPhase::QueueingUnfinishedWork,
            } => 2,
            Self::Queued => 3,
            Self::Running => 4,
            Self::Ready { .. } | Self::Blocked { .. } => 5,
        }
    }
    /// Plain bounded status copy; counts always name readiness targets and their phase.
    pub fn message(&self) -> String {
        let activity = match self {
            Self::Preparing { phase } => phase.message(),
            Self::Queued => "Feature work prepared",
            Self::Running => "Analyzing features",
            Self::Ready { .. } => "Features ready",
            Self::Blocked { reason } => return format!("Features blocked: {}", reason.message()),
        };
        match self.measurement() {
            FeaturePhaseMeasurement::Indeterminate => activity.to_owned(),
            FeaturePhaseMeasurement::Targets { completed, total } => {
                format!("{activity}: {completed} / {total} readiness targets")
            }
        }
    }
    /// Reduce one request's transient phases monotonically, allowing cache-hit/no-op and
    /// early blocked terminals. A retry or new source revision must start a new request;
    /// the caller still checks source/lifecycle/revision/request/path before this reducer.
    pub fn follows(&self, previous: Option<&Self>) -> bool {
        let Some(previous) = previous else {
            return true;
        };
        if matches!(previous, Self::Ready { .. } | Self::Blocked { .. }) {
            return false;
        }
        self.progress_rank() > previous.progress_rank()
    }
}
