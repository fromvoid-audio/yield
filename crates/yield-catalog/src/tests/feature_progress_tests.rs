//! Bounded progress aggregation remains observational; caller fences precede reduction.
use crate::{FeatureAnalysisPhase, FeatureAnalysisState, FeaturePhaseMeasurement};
use yield_library::sample_sources::SourceId;
#[test]
fn feature_counts_are_exact_target_units_and_unknown_or_mixed_sources_stay_indeterminate() {
    use FeatureAnalysisPhase::*;
    let aggregate = |states: &[FeatureAnalysisState]| {
        FeatureAnalysisState::aggregate_measurement(
            &states
                .iter()
                .enumerate()
                .map(|(i, state)| (SourceId::from_string(format!("source-{i}")), state.clone()))
                .collect::<Vec<_>>(),
        )
    };
    let comparing = FeatureAnalysisState::Preparing {
        phase: ComparingReadiness,
    };
    let inspecting = FeatureAnalysisState::Preparing {
        phase: InspectingManifest,
    };
    assert_eq!(
        FeatureAnalysisState::aggregate_measurement(&[
            (SourceId::from_string("same"), comparing.clone()),
            (SourceId::from_string("same"), comparing.clone())
        ]),
        FeaturePhaseMeasurement::Indeterminate
    );
    assert_eq!(
        comparing.measurement(),
        FeaturePhaseMeasurement::Targets {
            completed: 0,
            total: 1
        }
    );
    assert_eq!(
        aggregate(&[comparing.clone(), comparing.clone()]),
        FeaturePhaseMeasurement::Targets {
            completed: 0,
            total: 2
        }
    );
    assert_eq!(
        aggregate(&[comparing.clone(), inspecting.clone()]),
        FeaturePhaseMeasurement::Indeterminate
    );
    assert_eq!(
        aggregate(&[comparing, FeatureAnalysisState::Queued]),
        FeaturePhaseMeasurement::Indeterminate
    );
    assert_eq!(aggregate(&[]), FeaturePhaseMeasurement::Indeterminate);
    assert_eq!(
        aggregate(&vec![FeatureAnalysisState::Queued; 65]),
        FeaturePhaseMeasurement::Indeterminate
    );
    assert_eq!(
        aggregate(&[FeatureAnalysisState::Queued, FeatureAnalysisState::Queued]),
        FeaturePhaseMeasurement::Targets {
            completed: 2,
            total: 2
        }
    );
    assert_eq!(
        FeatureAnalysisState::Running.measurement(),
        FeaturePhaseMeasurement::Indeterminate
    );
    assert!(!inspecting.message().contains(" / "));
    assert!(
        FeatureAnalysisState::Queued
            .message()
            .contains("readiness targets")
    );
    assert!(!FeatureAnalysisState::Queued.message().contains("jobs"));
}
