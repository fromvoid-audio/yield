//! Source-bound transient computation and disposable cache reuse without a readiness claim.

use std::time::{Duration, Instant};

use yield_analysis::transients::{
    DETECTOR_VERSION, MAX_TRANSIENT_MARKERS, TransientMarkers, TransientPreset, detect_transients,
};
use yield_library::sample_sources::{
    SourceRevision, SourceWriter, SourceWriterError,
    db::transient_cache::{CachedTransientMarkers, TRANSIENT_CACHE_MAX_MARKERS},
    readiness::{SourceFeatureInput, SourceFeatureInputError},
    reconciliation::RootRelativePath,
};

use crate::FeatureAnalysisCancellation;

const _: () = assert!(MAX_TRANSIENT_MARKERS <= TRANSIENT_CACHE_MAX_MARKERS);

/// Exact Source input retained while a worker computes the result.
pub struct PreparedTransientJob {
    input: SourceFeatureInput,
    preset: TransientPreset,
    cancellation: FeatureAnalysisCancellation,
    cached: Option<CachedTransientMarkers>,
}

/// Computed result still awaiting validation on the original Source owner.
pub struct ComputedTransientJob {
    input: SourceFeatureInput,
    result: Result<(TransientMarkers, bool), TransientJobError>,
    cancellation: FeatureAnalysisCancellation,
    elapsed: Duration,
}

/// Whether a verified transient result was reused or computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransientCacheUse {
    /// A valid cached payload was reused after verifying the current audio bytes.
    Hit,
    /// Computed a fresh result and stored it for reuse.
    MissStored,
    /// Computed a fresh result while the cache write was unavailable.
    MissUnstored,
}

/// Exact native markers and measured compute/cache evidence.
#[derive(Debug)]
pub struct TransientJobOutcome {
    /// Owner-validated marker positions.
    pub markers: TransientMarkers,
    /// Observed cache path for this result.
    pub cache: TransientCacheUse,
    /// Background time for held media verification and any decode or detector work.
    pub elapsed: Duration,
}

/// Reason a transient job did not return a usable result.
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
    let cached = writer
        .lookup_transient_cache(&input, DETECTOR_VERSION, preset.key())
        .ok()
        .flatten();
    Ok(PreparedTransientJob {
        input,
        preset,
        cancellation,
        cached,
    })
}

impl PreparedTransientJob {
    /// Read and analyze on a compute worker without borrowing the Source writer.
    pub fn compute(self) -> ComputedTransientJob {
        let started = Instant::now();
        let cancel = self.cancellation.flag();
        let result = self
            .input
            .read_verified_bytes(cancel)
            .map_err(TransientJobError::Input)
            .and_then(|bytes| {
                if let Some(cached) = self.cached
                    && let Some(markers) = TransientMarkers::from_ordered_frames(
                        cached.source_frames(),
                        self.preset,
                        cached.into_frames(),
                    )
                {
                    return Ok((markers, true));
                }
                let pcm = yield_analysis::decode_wav_pcm(&bytes, cancel)
                    .map_err(TransientJobError::Decode)?;
                let markers = detect_transients(&pcm, self.preset, cancel)
                    .map_err(TransientJobError::Analysis)?;
                Ok((markers, false))
            });
        ComputedTransientJob {
            input: self.input,
            result,
            cancellation: self.cancellation,
            elapsed: started.elapsed(),
        }
    }
}

impl ComputedTransientJob {
    /// Return the result only while the original owner, revision, file and request are current.
    pub fn finish(self, writer: &SourceWriter) -> Result<TransientJobOutcome, TransientJobError> {
        if self.cancellation.is_cancelled() {
            return Err(TransientJobError::Cancelled);
        }
        writer.validate_audio_input(&self.input)?;
        if self.cancellation.is_cancelled() {
            return Err(TransientJobError::Cancelled);
        }
        let (markers, cache_hit) = self.result?;
        let cache = if cache_hit {
            TransientCacheUse::Hit
        } else {
            let stored =
                CachedTransientMarkers::new(markers.source_frames(), markers.frames().to_vec())
                    .is_some_and(|payload| {
                        writer
                            .store_transient_cache(
                                &self.input,
                                DETECTOR_VERSION,
                                markers.preset().key(),
                                &payload,
                                self.cancellation.flag(),
                            )
                            .unwrap_or(false)
                    });
            writer.validate_audio_input(&self.input)?;
            if stored {
                TransientCacheUse::MissStored
            } else {
                TransientCacheUse::MissUnstored
            }
        };
        if self.cancellation.is_cancelled() {
            return Err(TransientJobError::Cancelled);
        }
        Ok(TransientJobOutcome {
            markers,
            cache,
            elapsed: self.elapsed,
        })
    }
}
