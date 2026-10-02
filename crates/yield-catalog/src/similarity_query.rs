//! Exact bounded aspect queries over retained-owner validated feature bytes.
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use yield_analysis::aspects::{AspectDescriptorSet, SimilarityAspect};
use yield_library::operation_journal::StoredRelativePath;
use yield_library::sample_sources::db::{
    SourceFileObservation, SourceFileObservationError, SourceFileObservationKind,
};
use yield_library::sample_sources::readiness::{
    FeatureArtifactKey, ReadinessEligibility, ReadinessScopeKind, ReadinessStage, ReadinessTarget,
};
use yield_library::sample_sources::{SourceRevision, SourceWriter, SourceWriterError};

/// One explicit candidate set; this never implies whole-source similarity completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSimilarityQuery {
    revision: SourceRevision,
    anchor: ReadinessTarget,
    aspect: SimilarityAspect,
    generation: u64,
    candidates: Vec<ReadinessTarget>,
}
impl SourceSimilarityQuery {
    /// Validate bounds and identities before copying inputs or reading any payload.
    pub fn new(
        revision: SourceRevision,
        anchor: ReadinessTarget,
        aspect: SimilarityAspect,
        generation: u64,
        candidates: &[ReadinessTarget],
    ) -> Result<Self, SimilarityQueryError> {
        if generation == 0 || candidates.len() > 64 {
            return Err(SimilarityQueryError::InvalidInput);
        }
        let mut bytes = validate_target(&revision, &anchor)?;
        for (index, target) in candidates.iter().enumerate() {
            bytes += validate_target(&revision, target)?;
            if target.scope_id == anchor.scope_id
                || target.relative_path == anchor.relative_path
                || candidates[..index].iter().any(|previous| {
                    previous.scope_id == target.scope_id
                        || previous.relative_path == target.relative_path
                })
            {
                return Err(SimilarityQueryError::InvalidInput);
            }
        }
        if bytes > 64 * 1024 {
            return Err(SimilarityQueryError::InvalidInput);
        }
        Ok(Self {
            revision,
            anchor,
            aspect,
            generation,
            candidates: candidates.to_vec(),
        })
    }
    /// The exact anchor, including content and required feature version.
    pub fn anchor(&self) -> &ReadinessTarget {
        &self.anchor
    }
    /// Aspect used by the existing descriptor scorer.
    pub fn aspect(&self) -> SimilarityAspect {
        self.aspect
    }
    /// Consumer-issued nonzero query generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Full physical-owner, lifecycle and source revision fence.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
}

/// Bounded reason a query produced no publishable result.
#[derive(Debug, Error)]
pub enum SimilarityQueryError {
    /// Malformed, foreign, duplicate or over-budget input.
    #[error("invalid bounded similarity query")]
    InvalidInput,
    /// The exact source/lifecycle/lease/revision no longer matches.
    #[error("similarity source changed")]
    Stale,
    /// Cancellation prevents any result from being installed.
    #[error("similarity query cancelled")]
    Cancelled,
    /// Anchor payload is absent, corrupt, outdated or has no valid selected descriptor.
    #[error("similarity anchor features unavailable")]
    AnchorUnavailable,
    /// Current owner validation or stored payload lookup failed; retain existing evidence.
    #[error("similarity owner lookup failed: {0}")]
    Owner(#[from] SourceWriterError),
    /// Exact manifest identity or retained observation custody failed.
    #[error("similarity file observation failed: {0}")]
    Observation(#[from] SourceFileObservationError),
}
/// Each candidate retains a truthful outcome instead of invented readiness or scores.
#[derive(Clone, Debug, PartialEq)]
pub enum SimilarityCandidateOutcome {
    /// Finite cosine similarity from validated current v1 feature descriptors.
    Scored(f32),
    /// Exact current payload/completion or selected descriptor could not be validated.
    Unavailable,
}
/// One result row, retaining exact candidate/content and validated artifact identity.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceSimilarityRow {
    target: ReadinessTarget,
    key: Option<FeatureArtifactKey>,
    outcome: SimilarityCandidateOutcome,
}
impl SourceSimilarityRow {
    /// Exact candidate target for display binding; not a filesystem capability.
    pub fn target(&self) -> &ReadinessTarget {
        &self.target
    }
    /// Validated stored key when bytes were available; not a write grant.
    pub fn key(&self) -> Option<&FeatureArtifactKey> {
        self.key.as_ref()
    }
    /// Score or explicit unavailable result for this candidate only.
    pub fn outcome(&self) -> &SimilarityCandidateOutcome {
        &self.outcome
    }
}
/// Immutable owner-produced result; matching query context is necessary before presentation.
#[derive(Clone, Debug)]
pub struct SourceSimilarityResult {
    query: SourceSimilarityQuery,
    anchor_key: FeatureArtifactKey,
    rows: Vec<SourceSimilarityRow>,
    observations: Vec<SourceFileObservation>,
}
impl SourceSimilarityResult {
    /// Require exact source/root/lifecycle/lease/revision, anchor, aspect, generation and set.
    /// The consumer must also obtain current owner context; a saved request is not freshness.
    pub fn matches_query(&self, current: &SourceSimilarityQuery) -> bool {
        &self.query == current
    }
    /// Recheck exact captured file facts and retained owner liveness before installation/render.
    /// This is point-in-time metadata freshness, never physical input or overall readiness.
    pub fn is_current_for(&self, current: &SourceSimilarityQuery) -> bool {
        self.matches_query(current) && self.observations.iter().all(|file| file.validate().is_ok())
    }
    /// Exact anchor artifact used for scoring.
    pub fn anchor_key(&self) -> &FeatureArtifactKey {
        &self.anchor_key
    }
    /// At most 64 scored/unavailable rows; scored rows descend by cosine then path.
    pub fn rows(&self) -> &[SourceSimilarityRow] {
        &self.rows
    }
}

/// Read and score only the explicit set through the retained source writer.
/// Does not open SQL connections, publish targets, queue jobs or mark anything ready.
/// Cancellation and current owner revision are rechecked before returning any result.
pub fn query_source_similarity(
    writer: &mut SourceWriter,
    query: &SourceSimilarityQuery,
    cancel: &AtomicBool,
) -> Result<SourceSimilarityResult, SimilarityQueryError> {
    check_current(writer, query, cancel)?;
    let mut observations = vec![observe(writer, query, &query.anchor)?];
    let Some((anchor_key, anchor)) = descriptors(writer, query, &query.anchor)? else {
        return Err(SimilarityQueryError::AnchorUnavailable);
    };
    if !anchor.is_valid(query.aspect) {
        return Err(SimilarityQueryError::AnchorUnavailable);
    }
    let mut rows = Vec::with_capacity(query.candidates.len());
    for target in &query.candidates {
        check_cancel(cancel)?;
        observations.push(observe(writer, query, target)?);
        let (key, outcome) = match descriptors(writer, query, target)? {
            Some((key, descriptor)) => {
                let outcome = descriptor
                    .cosine_with(&anchor, query.aspect)
                    .filter(|score| score.is_finite())
                    .map(SimilarityCandidateOutcome::Scored)
                    .unwrap_or(SimilarityCandidateOutcome::Unavailable);
                (Some(key), outcome)
            }
            None => (None, SimilarityCandidateOutcome::Unavailable),
        };
        rows.push(SourceSimilarityRow {
            target: target.clone(),
            key,
            outcome,
        });
    }
    rows.sort_by(|left, right| {
        match (&left.outcome, &right.outcome) {
            (SimilarityCandidateOutcome::Scored(a), SimilarityCandidateOutcome::Scored(b)) => {
                b.total_cmp(a)
            }
            (SimilarityCandidateOutcome::Scored(_), _) => std::cmp::Ordering::Less,
            (_, SimilarityCandidateOutcome::Scored(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.target.relative_path.cmp(&right.target.relative_path))
    });
    check_current(writer, query, cancel)?;
    for observation in &observations {
        observation.validate()?;
    }
    Ok(SourceSimilarityResult {
        query: query.clone(),
        anchor_key,
        rows,
        observations,
    })
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), SimilarityQueryError> {
    if cancel.load(Ordering::Acquire) {
        Err(SimilarityQueryError::Cancelled)
    } else {
        Ok(())
    }
}
fn check_current(
    writer: &SourceWriter,
    query: &SourceSimilarityQuery,
    cancel: &AtomicBool,
) -> Result<(), SimilarityQueryError> {
    check_cancel(cancel)?;
    if writer.current_revision()? != query.revision {
        return Err(SimilarityQueryError::Stale);
    }
    Ok(())
}
fn descriptors(
    writer: &mut SourceWriter,
    query: &SourceSimilarityQuery,
    target: &ReadinessTarget,
) -> Result<Option<(FeatureArtifactKey, AspectDescriptorSet)>, SimilarityQueryError> {
    let Some((key, payload)) =
        writer.lookup_feature_artifact_for_target(&query.revision, target)?
    else {
        return Ok(None);
    };
    let vector: Vec<f32> = payload
        .bytes()
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("validated f32 bytes")))
        .collect();
    Ok(AspectDescriptorSet::from_feature_vector_v1(&vector)
        .ok()
        .map(|descriptor| (key, descriptor)))
}
fn observe(
    writer: &SourceWriter,
    query: &SourceSimilarityQuery,
    target: &ReadinessTarget,
) -> Result<SourceFileObservation, SimilarityQueryError> {
    let path = StoredRelativePath::new(target.relative_path.clone().expect("validated query path"))
        .map_err(|_| SimilarityQueryError::InvalidInput)?;
    let observation = writer.observe_file_identity(path)?;
    if observation.source_revision() != &query.revision
        || observation.kind() != SourceFileObservationKind::SupportedAudio
        || observation.file_identity() != Some(target.scope_id.as_str())
        || observation.content_hash() != Some(target.content_generation.as_str())
    {
        return Err(SimilarityQueryError::Stale);
    }
    Ok(observation)
}
fn validate_target(
    revision: &SourceRevision,
    target: &ReadinessTarget,
) -> Result<usize, SimilarityQueryError> {
    let Some(path) = &target.relative_path else {
        return Err(SimilarityQueryError::InvalidInput);
    };
    if target.source_id != revision.source_id().as_str()
        || target.scope_kind != ReadinessScopeKind::File
        || target.stage != ReadinessStage::AnalysisFeatures
        || target.required_version != "features-v1"
        || target.eligibility != ReadinessEligibility::Eligible
        || target.scope_id.is_empty()
        || target.scope_id.len() > 1024
        || target.content_generation.len() != 64
        || !target
            .content_generation
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || path.len() > 4096
        || path.is_empty()
        || StoredRelativePath::new(path.clone()).is_err()
    {
        return Err(SimilarityQueryError::InvalidInput);
    }
    Ok(path.len())
}
