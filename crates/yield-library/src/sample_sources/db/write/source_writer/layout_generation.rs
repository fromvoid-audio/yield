//! Owner-prepared bounded canonical generations; persisted bytes never acquire ownership.
use super::{SourceLayoutCapture, SourceLayoutCaptureError, SourceRevision, SourceWriter};
use crate::sample_sources::db::SourceReferenceFence;
use std::{
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
mod codec;
pub use codec::{SourceLayoutGeneration, SourceLayoutGenerationRow};

/// Exact metadata key reserved for the future admitted publication owner.
pub const SOURCE_LAYOUT_GENERATION_KEY: &str = "source_starmap_generation_v1";
/// Encoded canonical generation maximum; not a physical allocation grant.
pub const SOURCE_LAYOUT_GENERATION_MAX_BYTES: usize = 512 * 1024;

/// Borrowed, untrusted CPU observation; construction confers no owner authority.
#[derive(Debug, Clone, Copy)]
pub struct SourceLayoutComputedPoint<'a> {
    /// Exact ordered source_id::relative_path identity.
    pub sample_identity: &'a str,
    /// Raw finite existing-engine coordinates, before presentation normalization.
    pub coordinates: [f32; 2],
    /// Existing deterministic cluster label; absent below five rows.
    pub cluster_id: Option<i32>,
}
/// Borrowed result projection of the independent existing-engine helper.
#[derive(Debug)]
pub struct SourceLayoutComputedView<'a> {
    /// Exact fixed model/projection/cluster policy version.
    pub algorithm_version: &'a str,
    /// Independent fingerprint of ordered sample IDs and exact feature bytes.
    pub feature_input_digest: &'a [u8; 32],
    /// Complete exact ordered result; never partial-source coverage.
    pub points: &'a [SourceLayoutComputedPoint<'a>],
}
/// Private retained-owner prepared bytes, neither deserializable nor publicly constructible.
#[derive(Debug)]
pub struct SourcePreparedLayoutGeneration {
    fence: SourceReferenceFence,
    owner: Weak<()>,
    bytes: Vec<u8>,
    input_digest: [u8; 32],
    point_digest: [u8; 32],
    bytes_digest: [u8; 32],
}
impl SourcePreparedLayoutGeneration {
    /// Canonical historical bytes; these alone cannot authorize publication.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Full physical source/membership/feature identity.
    pub fn input_digest(&self) -> &[u8; 32] {
        &self.input_digest
    }
    /// Ordered exact coordinate/cluster content identity.
    pub fn point_digest(&self) -> &[u8; 32] {
        &self.point_digest
    }
    /// Exact encoded bytes identity, suitable only as an observed CAS value.
    pub fn bytes_digest(&self) -> &[u8; 32] {
        &self.bytes_digest
    }
    /// Historical preparation revision; current validation is separately required.
    pub fn revision(&self) -> &SourceRevision {
        self.fence.source_revision()
    }
    /// Read-only point-in-time issuer/ownership check, never mutation authority.
    pub fn validate_read_fence(&self) -> Result<(), SourceLayoutGenerationError> {
        self.fence
            .validate()
            .map_err(SourceLayoutCaptureError::from)?;
        Ok(())
    }
}
/// Explicit failed preparation or malformed archived evidence; no repair or write occurs.
#[derive(Debug, Error)]
pub enum SourceLayoutGenerationError {
    /// Actual owner/capture/read-fence prerequisite failed.
    #[error(transparent)]
    Capture(#[from] SourceLayoutCaptureError),
    /// Input/result/archived record violates the exact generation contract.
    #[error("invalid layout generation: {0}")]
    Invalid(&'static str),
    /// Checked finite allocation or encoding bound exceeded.
    #[error("layout generation exceeds {0}")]
    Limit(&'static str),
    /// Cooperative cancellation prevents a prepared success.
    #[error("layout generation cancelled")]
    Cancelled,
    /// Two-second cooperative owner budget including queue wait expired.
    #[error("layout generation deadline expired")]
    Deadline,
}
fn checkpoint(cancel: &AtomicBool, deadline: Instant) -> Result<(), SourceLayoutGenerationError> {
    if cancel.load(Ordering::Acquire) {
        return Err(SourceLayoutGenerationError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(SourceLayoutGenerationError::Deadline);
    }
    Ok(())
}
/// Fingerprint actual captured bytes with the independent CPU helper's exact v1 framing.
fn cpu_digest(capture: &SourceLayoutCapture) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"yield.starmap.cpu-input.v1\0");
    let algorithm = super::SOURCE_LAYOUT_ALGORITHM_V1.as_bytes();
    hash.update(&(algorithm.len() as u64).to_le_bytes());
    hash.update(algorithm);
    hash.update(&(capture.features().len() as u64).to_le_bytes());
    for row in capture.features() {
        hash.update(&(row.sample_identity().len() as u64).to_le_bytes());
        hash.update(row.sample_identity().as_bytes());
        hash.update(row.payload().bytes());
    }
    *hash.finalize().as_bytes()
}
impl SourceWriter {
    /// Prepare canonical bytes against complete actual current retained inputs.
    /// CPU results are observations only; no transaction, lease renewal, SQL writes,
    /// journal intent, admission or generation installation occurs here.
    pub fn prepare_layout_generation(
        &self,
        capture: &SourceLayoutCapture,
        computed: &SourceLayoutComputedView<'_>,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SourcePreparedLayoutGeneration, SourceLayoutGenerationError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        checkpoint(cancel, deadline)?;
        self.validate_layout_capture(capture, cancel, deadline)?;
        if computed.algorithm_version != super::SOURCE_LAYOUT_ALGORITHM_V1 {
            return Err(SourceLayoutGenerationError::Invalid("algorithm version"));
        }
        if computed.points.len() != capture.features().len() {
            return Err(SourceLayoutGenerationError::Invalid("complete coverage"));
        }
        let cpu = cpu_digest(capture);
        if &cpu != computed.feature_input_digest {
            return Err(SourceLayoutGenerationError::Invalid(
                "CPU feature fingerprint",
            ));
        }
        for (row, point) in capture.features().iter().zip(computed.points) {
            checkpoint(cancel, deadline)?;
            if row.sample_identity() != point.sample_identity {
                return Err(SourceLayoutGenerationError::Invalid(
                    "ordered sample identity",
                ));
            }
            codec::validate_point(point.coordinates, point.cluster_id, computed.points.len())?;
        }
        let bytes = codec::encode(capture, computed, &cpu)?;
        let decoded = SourceLayoutGeneration::decode(&bytes)?;
        let point_digest = *decoded.point_digest();
        let bytes_digest = *blake3::hash(&bytes).as_bytes();
        self.validate_layout_capture(capture, cancel, deadline)?;
        let fence = self
            .reference_read_fence()
            .map_err(SourceLayoutCaptureError::from)?;
        if fence.source_revision() != capture.revision() {
            return Err(SourceLayoutCaptureError::Stale.into());
        }
        checkpoint(cancel, deadline)?;
        Ok(SourcePreparedLayoutGeneration {
            fence,
            owner: std::sync::Arc::downgrade(&self.reference_liveness),
            bytes,
            input_digest: *capture.input_digest(),
            point_digest,
            bytes_digest,
        })
    }
    /// Recheck a prepared value using a fresh complete capture on its exact issuer.
    /// Requires transaction quiescence; future admitted store owns in-transaction checks.
    pub fn validate_prepared_layout_generation(
        &self,
        prepared: &SourcePreparedLayoutGeneration,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), SourceLayoutGenerationError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        checkpoint(cancel, deadline)?;
        if !prepared
            .owner
            .ptr_eq(&std::sync::Arc::downgrade(&self.reference_liveness))
        {
            return Err(SourceLayoutCaptureError::Stale.into());
        }
        prepared.validate_read_fence()?;
        let capture = self.capture_layout_features(prepared.revision(), cancel, deadline)?;
        if capture.input_digest() != &prepared.input_digest {
            return Err(SourceLayoutCaptureError::Stale.into());
        }
        checkpoint(cancel, deadline)
    }
}
