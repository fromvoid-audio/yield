//! Durable, versioned source-readiness targets and reconciliation.
//!
//! The source database owns desired targets and exact artifact completions. Actionable deficits
//! reuse the existing `analysis_jobs` table, while the legacy analysis claimant ignores rows owned
//! by the readiness coordinator until the bounded supervisor claims them explicitly.

mod feature_artifact;
mod feature_input;
mod model;
mod snapshot;
mod store;

pub use feature_artifact::{
    FeatureArtifactClaim, FeatureArtifactKey, FeatureArtifactPayload, FeatureArtifactPublication,
    FeatureArtifactRebuildAdmission,
};
pub(crate) use feature_input::InputPermit;
pub use feature_input::{
    FEATURE_INPUT_MAX_BYTES, PendingSourceContentInput, PreparedSourceContentDigest,
    SourceAudioStreamError, SourceFeatureInput, SourceFeatureInputError,
    TRANSIENT_STREAM_MAX_BYTES,
};
pub use model::{
    ClaimedReadinessWork, MAX_READINESS_CONTRACT_VERSION_BYTES, MAX_READINESS_TARGET_TEXT_BYTES,
    ReadinessArtifact, ReadinessClaimOrigin, ReadinessEligibility, ReadinessFailureClassification,
    ReadinessFailureOutcome, ReadinessLeaseRenewalOutcome, ReadinessMembership,
    ReadinessRetryPolicy, ReadinessScopeKind, ReadinessStage, ReadinessTarget,
    ReadinessWorkMutationOutcome, ReadinessWorkStats, SourceAvailability,
};
pub use snapshot::{
    ArtifactPublishOutcome, ReadinessActivity, ReadinessClassification, ReadinessDeficit,
    ReadinessEntry, ReadinessProgress, ReadinessSnapshot, ReadinessStageCounts,
};
#[cfg(test)]
pub(crate) use store::reconcile_readiness_with_hook;
pub(crate) use store::{FEATURE_ARTIFACT_SCHEMA, ReadinessWakeQualification};
pub use store::{
    ReadinessCompatibilityCleanup, ReadinessDeltaPublicationOutcome,
    ReadinessEmbeddingArtifactTarget, ReadinessError, ReadinessPublicationOrigin,
    ReadinessSimilarityManifest, ReadinessSimilarityManifestRequest,
    ReadinessSimilarityManifestRow, ReadinessSimilarityPayloadContract, ReadinessSourceState,
    ReadinessStore, ReadinessTargetDeltaPublication, ReadinessTargetPublication, ReadinessView,
    ReadinessWakeObservation,
};
#[cfg(test)]
pub(crate) use store::{
    cancel_readiness_work, claim_readiness_target, complete_readiness_work,
    complete_readiness_work_with_artifact_ref, fail_readiness_work, invalidate_readiness_artifact,
    persist_readiness_deficits, publish_readiness_artifact, readiness_work_stats,
    reconcile_readiness, release_readiness_work, renew_readiness_lease, replace_readiness_targets,
};

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub use feature_artifact::FeatureArtifactFixtureFault;
