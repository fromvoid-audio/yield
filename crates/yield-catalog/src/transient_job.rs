//! Source-bound transient computation and disposable cache reuse without a readiness claim.

use std::{
    io::Read,
    time::{Duration, Instant},
};

use yield_analysis::{
    WavPcmDecodeError,
    transients::{
        DETECTOR_VERSION, MAX_TRANSIENT_MARKERS, STREAM_DETECTOR_VERSION, TransientMarkers,
        TransientPreset, detect_streaming_transients, detect_transients,
    },
};
use yield_library::sample_sources::{
    SourceRevision, SourceWriter, SourceWriterError,
    db::transient_cache::{CachedTransientMarkers, TRANSIENT_CACHE_MAX_MARKERS},
    readiness::{
        FEATURE_INPUT_MAX_BYTES, SourceAudioStreamError, SourceFeatureInput,
        SourceFeatureInputError,
    },
    reconciliation::RootRelativePath,
};

use crate::FeatureAnalysisCancellation;

const _: () = assert!(MAX_TRANSIENT_MARKERS <= TRANSIENT_CACHE_MAX_MARKERS);

/// Exact Source input retained while a worker computes the result.
pub struct PreparedTransientJob {
    input: SourceFeatureInput,
    preset: TransientPreset,
    cancellation: FeatureAnalysisCancellation,
    primary: DetectorMode,
    cached: Option<(DetectorMode, CachedTransientMarkers)>,
}

/// Computed result still awaiting validation on the original Source owner.
pub struct ComputedTransientJob {
    input: SourceFeatureInput,
    result: Result<(TransientMarkers, DetectorMode, bool), TransientJobError>,
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
    /// Detector version used for these positions and their cache identity.
    pub detector_version: &'static str,
    /// Background time for held media verification and any decode or detector work.
    pub elapsed: Duration,
}

#[derive(Clone, Copy)]
enum DetectorMode {
    Whole,
    Streaming,
}

impl DetectorMode {
    fn version(self) -> &'static str {
        match self {
            Self::Whole => DETECTOR_VERSION,
            Self::Streaming => STREAM_DETECTOR_VERSION,
        }
    }
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
    let input = writer.prepare_transient_stream_input(revision, path)?;
    let (primary, versions) = if input.encoded_len() > FEATURE_INPUT_MAX_BYTES {
        (DetectorMode::Streaming, &[DetectorMode::Streaming][..])
    } else {
        (
            DetectorMode::Whole,
            &[DetectorMode::Whole, DetectorMode::Streaming][..],
        )
    };
    let cached = versions.iter().find_map(|&mode| {
        writer
            .lookup_transient_cache(&input, mode.version(), preset.key())
            .ok()
            .flatten()
            .map(|markers| (mode, markers))
    });
    Ok(PreparedTransientJob {
        input,
        preset,
        cancellation,
        primary,
        cached,
    })
}

impl PreparedTransientJob {
    /// Read and analyze on a compute worker without borrowing the Source writer.
    pub fn compute(mut self) -> ComputedTransientJob {
        let started = Instant::now();
        let result = self.compute_result();
        ComputedTransientJob {
            input: self.input,
            result,
            cancellation: self.cancellation,
            elapsed: started.elapsed(),
        }
    }

    fn compute_result(
        &mut self,
    ) -> Result<(TransientMarkers, DetectorMode, bool), TransientJobError> {
        if let Some((mode, cached)) = self.cached.take()
            && let Some(markers) = TransientMarkers::from_ordered_frames(
                cached.source_frames(),
                self.preset,
                cached.into_frames(),
            )
        {
            return self.verify_stream(|_| Ok((markers, mode, true)));
        }
        let cancel = self.cancellation.flag();
        if matches!(self.primary, DetectorMode::Streaming) {
            let markers = self
                .verify_stream(|reader| detect_streaming_transients(reader, self.preset, cancel))?;
            return Ok((markers, DetectorMode::Streaming, false));
        }
        let bytes = self.input.read_verified_bytes(cancel)?;
        match yield_analysis::decode_wav_pcm(&bytes, cancel) {
            Ok(pcm) => {
                let markers = detect_transients(&pcm, self.preset, cancel)
                    .map_err(TransientJobError::Analysis)?;
                Ok((markers, DetectorMode::Whole, false))
            }
            Err(WavPcmDecodeError::SampleLimit) => {
                let markers = detect_streaming_transients(bytes.as_slice(), self.preset, cancel)
                    .map_err(TransientJobError::Decode)?;
                Ok((markers, DetectorMode::Streaming, false))
            }
            Err(error) => Err(TransientJobError::Decode(error.message())),
        }
    }

    fn verify_stream<T>(
        &self,
        process: impl FnOnce(&mut dyn Read) -> Result<T, &'static str>,
    ) -> Result<T, TransientJobError> {
        self.input
            .with_verified_audio_reader(self.cancellation.flag(), process)
            .map_err(|error| match error {
                SourceAudioStreamError::Input(error) => TransientJobError::Input(error),
                SourceAudioStreamError::Processing(reason) => TransientJobError::Decode(reason),
            })
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
        let (markers, mode, cache_hit) = self.result?;
        let detector_version = mode.version();
        let cache = if cache_hit {
            TransientCacheUse::Hit
        } else {
            let stored =
                CachedTransientMarkers::new(markers.source_frames(), markers.frames().to_vec())
                    .is_some_and(|payload| {
                        writer
                            .store_transient_cache(
                                &self.input,
                                detector_version,
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
            detector_version,
            elapsed: self.elapsed,
        })
    }
}
