//! One bounded source-owned feature job backend. Scheduling/UI remain separate consumers.
use std::sync::atomic::{AtomicBool, Ordering};
use yield_library::sample_sources::readiness::{
    FeatureArtifactClaim, FeatureArtifactPayload, FeatureArtifactPublication,
    ReadinessFailureClassification, ReadinessRetryPolicy, ReadinessTarget, SourceFeatureInput,
    SourceFeatureInputError,
};
use yield_library::sample_sources::{SourceWriter, SourceWriterError};

/// Private retained source input and exact work claim; safe to move to a compute thread.
pub struct PreparedFeatureJob {
    claim: FeatureArtifactClaim,
    input: SourceFeatureInput,
}
/// Completed bounded CPU result, retaining source/input custody until owner publication.
pub struct ComputedFeatureJob {
    claim: FeatureArtifactClaim,
    input: SourceFeatureInput,
    result: Result<FeatureArtifactPayload, FeatureJobFailure>,
}

/// Bounded, typed reason why this job did not produce an artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureJobFailure {
    /// Source input changed, was unavailable or exceeded its admission bound.
    Input(SourceFeatureInputError),
    /// WAV decoding or feature extraction failed for the admitted content.
    Computation,
    /// Cancellation stopped computation before publication.
    Cancelled,
}
/// Observable job completion, with no optimistic ready state.
#[derive(Debug)]
pub enum FeatureJobOutcome {
    /// Exact payload/readiness result from the retained source transaction.
    Published(FeatureArtifactPublication),
    /// Failure was retained through the source readiness owner.
    Failed(FeatureJobFailure),
    /// Current claim was returned to pending with cancellation evidence.
    Cancelled,
    /// The lease/target changed; no completion, failure or cancellation was recorded.
    RejectedStale,
}

/// Claim and prepare exactly one current feature target. No data reads or CPU work occur here.
/// An input admission failure records a bounded retry reason through the same source owner.
pub fn prepare_feature_job(
    writer: &mut SourceWriter,
    target: &ReadinessTarget,
    now: i64,
    lease_seconds: i64,
) -> Result<Option<PreparedFeatureJob>, SourceWriterError> {
    let revision = writer.current_revision()?;
    let Some(claim) = writer.claim_feature_artifact(&revision, target, now, lease_seconds)? else {
        return Ok(None);
    };
    match writer.prepare_feature_input(&revision, &claim) {
        Ok(input) => Ok(Some(PreparedFeatureJob { claim, input })),
        Err(error) => {
            writer.fail_readiness_work(
                &revision,
                claim.readiness_claim(),
                ReadinessFailureClassification::Retryable,
                match &error {
                    SourceWriterError::FeatureInput(failure) => {
                        diagnostic(FeatureJobFailure::Input(*failure)).0
                    }
                    _ => "feature_input_admission",
                },
                "Feature input unavailable",
                now,
                ReadinessRetryPolicy::new(1, 60, 3).expect("bounded retry policy"),
            )?;
            Err(error)
        }
    }
}
impl PreparedFeatureJob {
    /// Read and compute outside the source worker/transaction. No database connection is borrowed.
    pub fn compute(self, cancel: &AtomicBool) -> ComputedFeatureJob {
        let result = if cancel.load(Ordering::Acquire) {
            Err(FeatureJobFailure::Cancelled)
        } else {
            self.input
                .read_verified_bytes(cancel)
                .map_err(|failure| match failure {
                    SourceFeatureInputError::Cancelled => FeatureJobFailure::Cancelled,
                    other => FeatureJobFailure::Input(other),
                })
                .and_then(|bytes| {
                    yield_analysis::compute_feature_vector_v1_for_wav_bytes(&bytes, cancel).map_err(
                        |_| {
                            if cancel.load(Ordering::Acquire) {
                                FeatureJobFailure::Cancelled
                            } else {
                                FeatureJobFailure::Computation
                            }
                        },
                    )
                })
                .and_then(|vector| {
                    FeatureArtifactPayload::from_v1_vector(&vector)
                        .map_err(|_| FeatureJobFailure::Computation)
                })
        };
        ComputedFeatureJob {
            claim: self.claim,
            input: self.input,
            result,
        }
    }
}
impl ComputedFeatureJob {
    /// Revalidate the held input/current owner, then atomically publish or retain failure evidence.
    pub fn finish(
        self,
        writer: &mut SourceWriter,
        now: i64,
        cancel: &AtomicBool,
    ) -> Result<FeatureJobOutcome, SourceWriterError> {
        let revision = writer.current_revision()?;
        writer.validate_feature_artifact_claim(&revision, &self.claim)?;
        if cancel.load(Ordering::Acquire) || self.result == Err(FeatureJobFailure::Cancelled) {
            let outcome = writer.cancel_readiness_work(
                &revision,
                self.claim.readiness_claim(),
                "Feature job cancelled",
                now,
            )?;
            return Ok(if outcome == yield_library::sample_sources::readiness::ReadinessWorkMutationOutcome::Recorded {
                FeatureJobOutcome::Cancelled
            } else { FeatureJobOutcome::RejectedStale });
        }
        let result = self
            .input
            .validate()
            .map_err(FeatureJobFailure::Input)
            .and(self.result);
        match result {
            Ok(payload) => {
                let outcome = writer.publish_feature_artifact(
                    &revision,
                    &self.claim,
                    &payload,
                    now,
                    cancel,
                )?;
                Ok(if outcome == FeatureArtifactPublication::RejectedStale {
                    FeatureJobOutcome::RejectedStale
                } else {
                    FeatureJobOutcome::Published(outcome)
                })
            }
            Err(failure) => {
                let outcome = writer.fail_readiness_work(
                    &revision,
                    self.claim.readiness_claim(),
                    ReadinessFailureClassification::Retryable,
                    diagnostic(failure).0,
                    diagnostic(failure).1,
                    now,
                    ReadinessRetryPolicy::new(1, 60, 3).expect("bounded retry policy"),
                )?;
                Ok(if outcome == yield_library::sample_sources::readiness::ReadinessFailureOutcome::RejectedStale {
                    FeatureJobOutcome::RejectedStale
                } else { FeatureJobOutcome::Failed(failure) })
            }
        }
    }
}

fn diagnostic(failure: FeatureJobFailure) -> (&'static str, &'static str) {
    match failure {
        FeatureJobFailure::Input(SourceFeatureInputError::ContentChanged) => {
            ("feature_content_changed", "Source content changed")
        }
        FeatureJobFailure::Input(SourceFeatureInputError::ContentPending) => (
            "feature_content_pending",
            "Source content digest is pending",
        ),
        FeatureJobFailure::Input(SourceFeatureInputError::UnsupportedPlatform) => (
            "feature_platform_unavailable",
            "Source input unavailable on this platform",
        ),
        FeatureJobFailure::Input(SourceFeatureInputError::ByteLimit) => {
            ("feature_input_limit", "Source input exceeds feature limit")
        }
        FeatureJobFailure::Input(SourceFeatureInputError::Busy) => {
            ("feature_input_busy", "Feature input busy")
        }
        FeatureJobFailure::Input(SourceFeatureInputError::Changed) => {
            ("feature_input_changed", "Source input changed")
        }
        FeatureJobFailure::Input(SourceFeatureInputError::Cancelled)
        | FeatureJobFailure::Cancelled => ("feature_cancelled", "Feature job cancelled"),
        FeatureJobFailure::Computation => {
            ("feature_computation_failed", "Feature computation failed")
        }
    }
}
