use super::*;

fn fixture(count: usize) -> (Vec<String>, Vec<Vec<f32>>) {
    let ids = (0..count)
        .map(|i| format!("source::sample-{i}.wav"))
        .collect();
    let features = (0..count)
        .map(|i| {
            (0..183)
                .map(|j| {
                    let x = (i * 193 + j * 17 + 1) as f32;
                    1.0 + (x * 0.137).sin() + (x * 0.031).cos()
                })
                .collect()
        })
        .collect();
    (ids, features)
}

fn borrowed<'a>(ids: &'a [String], features: &'a [Vec<f32>]) -> Vec<StarmapFeatureInput<'a>> {
    ids.iter()
        .zip(features)
        .map(|(id, features)| StarmapFeatureInput {
            sample_id: id,
            features,
        })
        .collect()
}

#[test]
fn empty_and_small_inputs_preserve_existing_math_without_clusters() {
    let cancel = AtomicBool::new(false);
    assert_eq!(
        compute_starmap_layout(&[], &cancel),
        Err(StarmapComputationError::Empty)
    );
    let (ids, features) = fixture(2);
    let inputs = borrowed(&ids, &features);
    let one = compute_starmap_layout(&inputs[..1], &cancel).unwrap();
    assert_eq!(one.points()[0].coordinates(), [0.0, 0.0]);
    let two = compute_starmap_layout(&inputs, &cancel).unwrap();
    assert_eq!(two.points()[0].coordinates(), [-1.0, 0.0]);
    assert_eq!(two.points()[1].coordinates(), [1.0, 0.0]);
    assert!(two.points().iter().all(|p| p.cluster_id().is_none()));
}

#[test]
fn actual_engines_are_complete_repeatable_and_bound_to_ordered_features() {
    let (ids, features) = fixture(16);
    let mut inputs = borrowed(&ids, &features);
    let cancel = AtomicBool::new(false);
    let first = compute_starmap_layout(&inputs, &cancel).unwrap();
    let second = compute_starmap_layout(&inputs, &cancel).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.algorithm_version(), SOURCE_LAYOUT_ALGORITHM_V1);
    for (point, id) in first.points().iter().zip(&ids) {
        assert_eq!(point.sample_id(), id);
        assert!(point.coordinates().iter().all(|v| v.is_finite()));
        assert!(point.cluster_id().is_some_and(|label| label >= 0));
    }
    inputs.swap(0, 1);
    assert_ne!(fingerprint(&inputs), *first.feature_input_digest());
    let mut changed = features.clone();
    changed[0][0] += 0.01;
    assert_ne!(
        fingerprint(&borrowed(&ids, &changed)),
        *first.feature_input_digest()
    );
    let mut renamed = ids.clone();
    renamed[0].push('x');
    assert_ne!(
        fingerprint(&borrowed(&renamed, &features)),
        *first.feature_input_digest()
    );
}

#[test]
fn maximum_workload_produces_every_finite_point_within_owned_bound() {
    let (_, features) = fixture(STARMAP_COMPUTE_MAX_SAMPLES);
    // A valid capture's manifest charge excludes its repeated source prefix.
    // These paths + short file IDs + 64-byte hashes fit its 128 KiB manifest;
    // the CPU identities are larger and must still compute the complete source.
    let ids: Vec<_> = (0..STARMAP_COMPUTE_MAX_SAMPLES)
        .map(|i| format!("{}::{}{i:03}.wav", "s".repeat(256), "p".repeat(400)))
        .collect();
    let manifest_bytes: usize = ids.iter().map(|id| id.len() - 258 + 8 + 64).sum();
    assert!(manifest_bytes < 128 * 1024);
    assert!(ids.iter().map(String::len).sum::<usize>() > 128 * 1024);
    let start = std::time::Instant::now();
    let result =
        compute_starmap_layout(&borrowed(&ids, &features), &AtomicBool::new(false)).unwrap();
    eprintln!(
        "maximum workload: {} samples, elapsed {:?}, direct buffer bound {} bytes",
        result.points().len(),
        start.elapsed(),
        result.owned_buffer_bound()
    );
    assert_eq!(result.points().len(), STARMAP_COMPUTE_MAX_SAMPLES);
    assert!(result.owned_buffer_bound() <= STARMAP_COMPUTE_MAX_OWNED_BYTES);
    for (point, id) in result.points().iter().zip(&ids) {
        assert_eq!(point.sample_id(), id);
        assert!(point.coordinates().iter().all(|v| v.is_finite()));
        assert!(point.cluster_id().is_some());
    }
}

#[test]
fn small_projection_keeps_existing_engine_errors_and_optional_clusters() {
    let (ids, features) = fixture(4);
    let inputs = borrowed(&ids, &features);
    let cancel = AtomicBool::new(false);
    // Three samples cannot meet the current engine's minimum perplexity rule.
    assert_eq!(
        compute_starmap_layout(&inputs[..3], &cancel),
        Err(StarmapComputationError::Unavailable)
    );
    let four = compute_starmap_layout(&inputs, &cancel).unwrap();
    assert_eq!(four.points().len(), 4);
    assert!(four.points().iter().all(|p| p.cluster_id().is_none()));
}

#[test]
fn malformed_vectors_and_identities_fail_before_entering_engines() {
    let (ids, features) = fixture(2);
    let cancel = AtomicBool::new(false);
    let invalid_ids = ["", "  ", "bad\0id"];
    for id in invalid_ids {
        assert_eq!(
            compute_starmap_layout(
                &[StarmapFeatureInput {
                    sample_id: id,
                    features: &features[0]
                }],
                &cancel
            ),
            Err(StarmapComputationError::InvalidIdentity)
        );
    }
    let oversized = "a".repeat(STARMAP_COMPUTE_MAX_SAMPLE_ID_BYTES + 1);
    assert_eq!(
        compute_starmap_layout(
            &[StarmapFeatureInput {
                sample_id: &oversized,
                features: &features[0]
            }],
            &cancel
        ),
        Err(StarmapComputationError::InvalidIdentity)
    );
    let mut inputs = borrowed(&ids, &features);
    inputs[1].sample_id = inputs[0].sample_id;
    assert_eq!(
        compute_starmap_layout(&inputs, &cancel),
        Err(StarmapComputationError::InvalidIdentity)
    );
    for invalid in [
        vec![1.0; 182],
        vec![0.0; 183],
        vec![f32::NAN; 183],
        vec![f32::INFINITY; 183],
        vec![f32::MAX; 183],
        vec![f32::MIN_POSITIVE; 183],
    ] {
        assert_eq!(
            compute_starmap_layout(
                &[StarmapFeatureInput {
                    sample_id: "valid",
                    features: &invalid
                }],
                &cancel
            ),
            Err(StarmapComputationError::InvalidFeatures)
        );
    }
}

#[test]
fn row_and_aggregate_identity_limits_reject_without_projection() {
    let (ids, features) = fixture(STARMAP_COMPUTE_MAX_SAMPLES + 1);
    let cancel = AtomicBool::new(false);
    assert_eq!(
        compute_starmap_layout(&borrowed(&ids, &features), &cancel),
        Err(StarmapComputationError::InputLimit)
    );
    let long_ids: Vec<_> = (0..256)
        .map(|i| format!("{i:03}{}", "x".repeat(800)))
        .collect();
    assert_eq!(
        compute_starmap_layout(&borrowed(&long_ids, &features[..256]), &cancel),
        Err(StarmapComputationError::InputLimit)
    );
}

#[test]
fn cancellation_discards_work_after_each_real_uninterruptible_stage() {
    let (ids, features) = fixture(8);
    let inputs = borrowed(&ids, &features);
    assert_eq!(
        compute_starmap_layout(&inputs, &AtomicBool::new(true)),
        Err(StarmapComputationError::Cancelled)
    );
    for target in [Stage::Embedded, Stage::Projected, Stage::Clustered] {
        let cancel = AtomicBool::new(false);
        let result = compute(&inputs, &cancel, |stage| {
            if std::mem::discriminant(&stage) == std::mem::discriminant(&target) {
                cancel.store(true, Ordering::Release);
            }
        });
        assert_eq!(result, Err(StarmapComputationError::Cancelled));
    }
}
