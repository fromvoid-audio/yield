//! Bounded computation only: source capture, admission and publication belong to retained owners.

use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

use super::{hdbscan, similarity, umap};

/// Exact algorithm policy shared with the retained source capture contract.
pub const SOURCE_LAYOUT_ALGORITHM_V1: &str =
    "features_v1__len183__l2/pca-tsne-v1/seed0/hdbscan-v1:min5,samples=none,single=false";
/// Initial complete-source computation limit; this is not large-map qualification.
pub const STARMAP_COMPUTE_MAX_SAMPLES: usize = 256;
/// Maximum UTF-8 bytes in a source ID, separator and relative sample path.
pub const STARMAP_COMPUTE_MAX_SAMPLE_ID_BYTES: usize = 1_282;
/// Aggregate sample identity bytes accepted before any input copying.
pub const STARMAP_COMPUTE_MAX_IDENTITY_BYTES: usize = 128 * 1024;
/// Conservative directly owned buffers, excluding existing engines' internal workspace.
pub const STARMAP_COMPUTE_MAX_OWNED_BYTES: usize = 1024 * 1024;

/// Borrowed immutable CPU input; this carries no source or SQL authority.
#[derive(Clone, Copy, Debug)]
pub struct StarmapFeatureInput<'a> {
    /// Exact source-local analysis identity, supplied in the capture's order.
    pub sample_id: &'a str,
    /// Exactly 183 finite, nonzero-normalizable v1 features.
    pub features: &'a [f32],
}

/// Bounded semantic failures; engine diagnostics and raw identities do not escape.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StarmapComputationError {
    /// Empty input has no publishable layout.
    #[error("starmap input is empty")]
    Empty,
    /// Row, identity or checked owned-buffer limits were exceeded.
    #[error("starmap input exceeds the computation bound")]
    InputLimit,
    /// A sample identity is empty, oversized, contains NUL or is duplicated.
    #[error("invalid starmap sample identity")]
    InvalidIdentity,
    /// A vector has the wrong shape, nonfinite elements or an unusable L2 norm.
    #[error("invalid starmap feature vector")]
    InvalidFeatures,
    /// Existing projection or clustering could not provide complete finite output.
    #[error("starmap computation unavailable")]
    Unavailable,
    /// Observed cancellation discards the result without publishing anything.
    #[error("starmap computation cancelled")]
    Cancelled,
}

/// One immutable engine-produced point; neither it nor its identity grants an action.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStarmapPoint {
    sample_id: String,
    coordinates: [f32; 2],
    cluster_id: Option<i32>,
}
impl ComputedStarmapPoint {
    /// Exact identity of the corresponding captured feature input.
    pub fn sample_id(&self) -> &str {
        &self.sample_id
    }
    /// Raw finite engine coordinates; the persisted reader owns reference normalization.
    pub const fn coordinates(&self) -> [f32; 2] {
        self.coordinates
    }
    /// Existing-engine cluster grouping, absent for fewer than five inputs.
    pub const fn cluster_id(&self) -> Option<i32> {
        self.cluster_id
    }
}

/// Immutable computation, not a source publication receipt or readiness observation.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStarmapLayout {
    points: Vec<ComputedStarmapPoint>,
    feature_input_digest: [u8; 32],
    owned_buffer_bound: usize,
}
impl ComputedStarmapLayout {
    /// Exact fixed algorithm/version and parameters used by this computation.
    pub const fn algorithm_version(&self) -> &'static str {
        SOURCE_LAYOUT_ALGORITHM_V1
    }
    /// Points in the exact input order, with one point per validated identity.
    pub fn points(&self) -> &[ComputedStarmapPoint] {
        &self.points
    }
    /// Digest of algorithm, ordered identities and exact little-endian feature values.
    /// This is separate from the owner's full physical/membership capture digest.
    pub const fn feature_input_digest(&self) -> &[u8; 32] {
        &self.feature_input_digest
    }
    /// Conservative directly owned-buffer bound; not a peak process-memory claim.
    pub const fn owned_buffer_bound(&self) -> usize {
        self.owned_buffer_bound
    }
}

/// Compute through the existing L2/PCA-t-SNE/HDBSCAN pipeline without I/O or publication.
///
/// Validate all inputs before copying or entering an engine. Cancellation is cooperative
/// between stages: existing engines cannot be interrupted mid-call. The caller must keep
/// the real source capture in custody until return and revalidate it before accepting or
/// publishing this result. A digest or this result cannot reacquire a source capability.
pub fn compute_starmap_layout(
    inputs: &[StarmapFeatureInput<'_>],
    cancel: &AtomicBool,
) -> Result<ComputedStarmapLayout, StarmapComputationError> {
    compute(inputs, cancel, |_| {})
}

#[derive(Clone, Copy)]
enum Stage {
    Embedded,
    Projected,
    Clustered,
}

fn compute(
    inputs: &[StarmapFeatureInput<'_>],
    cancel: &AtomicBool,
    stage_completed: impl Fn(Stage),
) -> Result<ComputedStarmapLayout, StarmapComputationError> {
    let owned_buffer_bound = validate(inputs, cancel)?;
    let feature_input_digest = fingerprint(inputs);
    let mut embeddings = Vec::with_capacity(inputs.len());
    for input in inputs {
        checkpoint(cancel)?;
        let embedding = similarity::embedding_from_features(input.features)
            .map_err(|_| StarmapComputationError::InvalidFeatures)?;
        embeddings.push((input.sample_id.to_owned(), embedding));
    }
    stage_completed(Stage::Embedded);
    checkpoint(cancel)?;
    // The existing implementation keeps its small-manifest behavior and fixed seed.
    // Require 100% finite coverage, rather than the compatibility bundle's 95%.
    let layout = umap::compute_layout_for_embeddings(&embeddings, 0, 1.0)
        .map_err(|_| StarmapComputationError::Unavailable)?;
    stage_completed(Stage::Projected);
    checkpoint(cancel)?;
    if layout.len() != inputs.len() || layout.iter().any(|p| p.iter().any(|v| !v.is_finite())) {
        return Err(StarmapComputationError::Unavailable);
    }
    let sample_ids = embeddings
        .into_iter()
        .map(|(identity, _)| identity)
        .collect::<Vec<_>>();
    let labels = if inputs.len() >= 5 {
        let labels = hdbscan::compute_layout_clusters(
            &sample_ids,
            &layout,
            hdbscan::HdbscanConfig {
                min_cluster_size: 5,
                min_samples: None,
                allow_single_cluster: false,
            },
        )
        .map_err(|_| StarmapComputationError::Unavailable)?;
        if labels.len() != inputs.len() || labels.iter().any(|label| *label < 0) {
            return Err(StarmapComputationError::Unavailable);
        }
        labels.into_iter().map(Some).collect::<Vec<_>>()
    } else {
        vec![None; inputs.len()]
    };
    stage_completed(Stage::Clustered);
    checkpoint(cancel)?;
    let points = sample_ids
        .into_iter()
        .zip(layout)
        .zip(labels)
        .map(
            |((sample_id, coordinates), cluster_id)| ComputedStarmapPoint {
                sample_id,
                coordinates,
                cluster_id,
            },
        )
        .collect();
    Ok(ComputedStarmapLayout {
        points,
        feature_input_digest,
        owned_buffer_bound,
    })
}

fn checkpoint(cancel: &AtomicBool) -> Result<(), StarmapComputationError> {
    if cancel.load(Ordering::Acquire) {
        Err(StarmapComputationError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate(
    inputs: &[StarmapFeatureInput<'_>],
    cancel: &AtomicBool,
) -> Result<usize, StarmapComputationError> {
    checkpoint(cancel)?;
    if inputs.is_empty() {
        return Err(StarmapComputationError::Empty);
    }
    if inputs.len() > STARMAP_COMPUTE_MAX_SAMPLES {
        return Err(StarmapComputationError::InputLimit);
    }
    let mut identity_bytes = 0usize;
    for (index, input) in inputs.iter().enumerate() {
        checkpoint(cancel)?;
        if input.sample_id.trim().is_empty()
            || input.sample_id.len() > STARMAP_COMPUTE_MAX_SAMPLE_ID_BYTES
            || input.sample_id.contains('\0')
            || inputs[..index]
                .iter()
                .any(|prior| prior.sample_id == input.sample_id)
        {
            return Err(StarmapComputationError::InvalidIdentity);
        }
        identity_bytes = identity_bytes
            .checked_add(input.sample_id.len())
            .filter(|bytes| *bytes <= STARMAP_COMPUTE_MAX_IDENTITY_BYTES)
            .ok_or(StarmapComputationError::InputLimit)?;
        if input.features.len() != similarity::SIMILARITY_DIM
            || input.features.iter().any(|v| !v.is_finite())
        {
            return Err(StarmapComputationError::InvalidFeatures);
        }
        // Match the existing f32 normalization's admissible norm before it allocates.
        let norm = input.features.iter().fold(0.0_f32, |sum, v| sum + v * v);
        if !norm.is_finite() || norm <= 0.0 {
            return Err(StarmapComputationError::InvalidFeatures);
        }
    }
    // Conservative overlap of three identity copies, f32 embeddings, the flattened
    // f64 matrix, tuple/vector/output headers, raw points and clustering input/labels.
    // Engine-owned PCA/t-SNE/HDBSCAN workspace is bounded by the shapes and measured
    // separately; it is not concealed inside this directly owned-buffer estimate.
    let per_row = similarity::SIMILARITY_DIM
        .checked_mul(12)
        .and_then(|bytes| bytes.checked_add(160))
        .ok_or(StarmapComputationError::InputLimit)?;
    identity_bytes
        .checked_mul(3)
        .and_then(|bytes| inputs.len().checked_mul(per_row)?.checked_add(bytes))
        .filter(|bytes| *bytes <= STARMAP_COMPUTE_MAX_OWNED_BYTES)
        .ok_or(StarmapComputationError::InputLimit)
}

fn fingerprint(inputs: &[StarmapFeatureInput<'_>]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"yield.starmap.cpu-input.v1\0");
    hash.update(&(SOURCE_LAYOUT_ALGORITHM_V1.len() as u64).to_le_bytes());
    hash.update(SOURCE_LAYOUT_ALGORITHM_V1.as_bytes());
    hash.update(&(inputs.len() as u64).to_le_bytes());
    for input in inputs {
        hash.update(&(input.sample_id.len() as u64).to_le_bytes());
        hash.update(input.sample_id.as_bytes());
        for value in input.features {
            hash.update(&value.to_le_bytes());
        }
    }
    *hash.finalize().as_bytes()
}

#[cfg(test)]
mod tests;
