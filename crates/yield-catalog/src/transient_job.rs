//! Source-bound, cancellable transient computation without a durable readiness claim.

use yield_analysis::transients::{TransientMarkers, TransientPreset, detect_transients};
use yield_library::sample_sources::{
    SourceRevision, SourceWriter, SourceWriterError,
    readiness::{SourceFeatureInput, SourceFeatureInputError},
    reconciliation::RootRelativePath,
};

use crate::FeatureAnalysisCancellation;

/// Exact Source input retained while a worker computes the result.
pub struct PreparedTransientJob {
    input: SourceFeatureInput,
    preset: TransientPreset,
    cancellation: FeatureAnalysisCancellation,
}

/// Computed result still awaiting validation on the original Source owner.
pub struct ComputedTransientJob {
    input: SourceFeatureInput,
    result: Result<TransientMarkers, TransientJobError>,
    cancellation: FeatureAnalysisCancellation,
}

/// Reason a read-only transient job did not return a usable result.
#[derive(Debug, thiserror::Error)]
pub enum TransientJobError {
    /// The owner could not admit or revalidate the exact Source entry.
    #[error("Transient analysis Source unavailable")]
    Source(#[from] SourceWriterError),
    /// Bounded verified input could not be read.
    #[error(transparent)]
    Input(#[from] SourceFeatureInputError),
    /// Native-rate PCM could not be decoded under the WAV input contract.
    #[error("Transient analysis WAV decode failed: {0}")]
    Decode(&'static str),
    /// The detector could not produce a result.
    #[error("Transient analysis failed: {0}")]
    Analysis(&'static str),
    /// Cancellation prevented the result from being delivered.
    #[error("Transient analysis cancelled")]
    Cancelled,
}

/// Capture one exact audited audio entry on its retained Source writer.
pub fn prepare_transient_job(
    writer: &SourceWriter,
    revision: &SourceRevision,
    path: &RootRelativePath,
    preset: TransientPreset,
    cancellation: FeatureAnalysisCancellation,
) -> Result<PreparedTransientJob, TransientJobError> {
    if cancellation.is_cancelled() {
        return Err(TransientJobError::Cancelled);
    }
    let input = writer.prepare_audio_input(revision, path)?;
    Ok(PreparedTransientJob {
        input,
        preset,
        cancellation,
    })
}

impl PreparedTransientJob {
    /// Read and analyze on a compute worker without borrowing the Source writer.
    pub fn compute(self) -> ComputedTransientJob {
        let cancel = self.cancellation.flag();
        let result = self
            .input
            .read_verified_bytes(cancel)
            .map_err(TransientJobError::Input)
            .and_then(|bytes| {
                yield_analysis::decode_wav_pcm(&bytes, cancel).map_err(TransientJobError::Decode)
            })
            .and_then(|pcm| {
                detect_transients(&pcm, self.preset, cancel).map_err(TransientJobError::Analysis)
            });
        ComputedTransientJob {
            input: self.input,
            result,
            cancellation: self.cancellation,
        }
    }
}

impl ComputedTransientJob {
    /// Return the result only while the original owner, revision, file and request are current.
    pub fn finish(self, writer: &SourceWriter) -> Result<TransientMarkers, TransientJobError> {
        if self.cancellation.is_cancelled() {
            return Err(TransientJobError::Cancelled);
        }
        writer.validate_audio_input(&self.input)?;
        if self.cancellation.is_cancelled() {
            return Err(TransientJobError::Cancelled);
        }
        self.result
    }
}
