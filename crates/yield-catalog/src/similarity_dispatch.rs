//! Bounded read-only aspect requests on the existing catalog owner.
use super::*;
use yield_library::operation_journal::StoredRelativePath;
use yield_library::sample_sources::db::SourceFileObservationKind;
use yield_library::sample_sources::readiness::{
    ReadinessEligibility, ReadinessScopeKind, ReadinessStage, ReadinessTarget,
};

// Covers two target/path copies, observations, validated keys and fixed row overhead.
// Each owner identity is capped at 1024 bytes before result construction. Shared
// publication custody is retained by Arc/Weak and is not copied per result.
pub(super) const MAX_RESULT_BYTES: usize = 1024 * 1024;

// Keep the reservation unchanged as result rows grow. Four path copies share the
// admitted 64 KiB text budget. Per file, 10 KiB conservatively covers three target
// identity copies, revision identities, observation facts/publication identity and
// fixed key/reference text (all bounded before construction). Structural sizes
// include the five inline Option<f32> scores and vector elements, not shared custody.
const RESULT_RETAINED_BYTES_BOUND: usize = 4 * 64 * 1024
    + 65 * (10 * 1024
        + std::mem::size_of::<SourceSimilarityRow>()
        + std::mem::size_of::<ReadinessTarget>()
        + std::mem::size_of::<yield_library::sample_sources::db::SourceFileObservation>())
    + std::mem::size_of::<SourceSimilarityResult>()
    + std::mem::size_of::<SourceSimilarityRequest>();
const _: () = assert!(RESULT_RETAINED_BYTES_BOUND <= MAX_RESULT_BYTES);

/// Immutable admitted request; explicit candidates never imply whole-library completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSimilarityRequest {
    source_id: SourceId,
    lifecycle: u64,
    revision: u64,
    generation: u64,
    anchor: String,
    candidates: Vec<String>,
    aspect: SimilarityAspect,
}
impl SourceSimilarityRequest {
    /// Validate the explicit set before cloning it or admitting a worker command.
    pub fn new(
        source_id: SourceId,
        lifecycle: u64,
        revision: u64,
        generation: u64,
        anchor: String,
        candidates: &[String],
        aspect: SimilarityAspect,
    ) -> Result<Self, SimilarityDispatchFailure> {
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || lifecycle == 0
            || generation == 0
            || candidates.len() > 64
        {
            return Err(SimilarityDispatchFailure::InvalidInput);
        }
        let valid_path =
            |path: &String| path.len() <= 4096 && StoredRelativePath::new(path.clone()).is_ok();
        if !valid_path(&anchor) {
            return Err(SimilarityDispatchFailure::InvalidInput);
        }
        let mut bytes = anchor.len();
        for (index, path) in candidates.iter().enumerate() {
            bytes = bytes.saturating_add(path.len());
            if !valid_path(path) || path == &anchor || candidates[..index].contains(path) {
                return Err(SimilarityDispatchFailure::InvalidInput);
            }
        }
        if bytes > 64 * 1024 {
            return Err(SimilarityDispatchFailure::InvalidInput);
        }
        Ok(Self {
            source_id,
            lifecycle,
            revision,
            generation,
            anchor,
            candidates: candidates.to_vec(),
            aspect,
        })
    }
    /// Source selected by the consumer.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    /// Registered lifecycle captured by the consumer.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle
    }
    /// Manifest revision captured by the consumer.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Nonzero consumer query generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Exact relative anchor pathname.
    pub fn anchor(&self) -> &str {
        &self.anchor
    }
    /// Exact explicitly supplied candidate set, in request order.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }
    /// Existing descriptor aspect to score.
    pub fn aspect(&self) -> SimilarityAspect {
        self.aspect
    }
    pub(super) fn input_bytes(&self) -> usize {
        self.source_id.as_str().len()
            + self.anchor.len()
            + self.candidates.iter().map(String::len).sum::<usize>()
            + self.candidates.len() * std::mem::size_of::<String>()
            + std::mem::size_of::<Self>()
    }
}
/// Cancellation retained through computation and queued delivery.
#[derive(Clone, Debug, Default)]
pub struct SimilarityCancellation(Arc<AtomicBool>);
impl SimilarityCancellation {
    /// Create one uncancelled query token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Reject computation and any queued projection for this token.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Cheap in-memory check; no SQL or filesystem activity.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// Bounded, non-sensitive failure; never an optimistic score or Ready state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimilarityDispatchFailure {
    /// Malformed or over-budget input/owner identity.
    InvalidInput,
    /// Registered owner or exact source revision changed.
    StaleSource,
    /// Anchor features or descriptor are unavailable.
    AnchorUnavailable,
    /// Exact file metadata/content identity is unavailable.
    IdentityUnavailable,
    /// Validated owner artifact lookup failed.
    OwnerUnavailable,
    /// Explicit cancellation stopped computation.
    Cancelled,
}

fn execute(
    writer: &mut SourceWriter,
    request: &SourceSimilarityRequest,
    cancellation: &SimilarityCancellation,
) -> Result<SourceSimilarityResult, SimilarityDispatchFailure> {
    let revision = writer
        .current_revision()
        .map_err(|_| SimilarityDispatchFailure::StaleSource)?;
    if revision.source_id() != &request.source_id
        || revision.lifecycle_generation() != request.lifecycle
        || revision.revision() != request.revision
    {
        return Err(SimilarityDispatchFailure::StaleSource);
    }
    if revision.root_identity().as_bytes().len() > 1024
        || revision.root_binding_identity().len() > 1024
        || revision.database_identity().len() > 1024
    {
        return Err(SimilarityDispatchFailure::InvalidInput);
    }
    let target = |path: &str| {
        if cancellation.is_cancelled() {
            return Err(SimilarityDispatchFailure::Cancelled);
        }
        let path = StoredRelativePath::new(path.to_owned())
            .map_err(|_| SimilarityDispatchFailure::InvalidInput)?;
        let file = writer
            .observe_file_identity(path.clone())
            .map_err(|_| SimilarityDispatchFailure::IdentityUnavailable)?;
        if file.source_revision() != &revision
            || file.kind() != SourceFileObservationKind::SupportedAudio
        {
            return Err(SimilarityDispatchFailure::StaleSource);
        }
        let object = file
            .file_identity()
            .filter(|id| !id.is_empty() && id.len() <= 1024)
            .ok_or(SimilarityDispatchFailure::IdentityUnavailable)?;
        let hash = file
            .content_hash()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or(SimilarityDispatchFailure::IdentityUnavailable)?;
        Ok(ReadinessTarget {
            source_id: request.source_id.as_str().to_owned(),
            source_generation: i64::try_from(revision.revision())
                .map_err(|_| SimilarityDispatchFailure::InvalidInput)?,
            scope_kind: ReadinessScopeKind::File,
            scope_id: object.to_owned(),
            stage: ReadinessStage::AnalysisFeatures,
            content_generation: hash.to_owned(),
            required_version: "features-v1".to_owned(),
            relative_path: Some(path.as_str().to_owned()),
            eligibility: ReadinessEligibility::Eligible,
        })
    };
    let anchor = target(&request.anchor)?;
    let candidates = request
        .candidates
        .iter()
        .map(|path| target(path))
        .collect::<Result<Vec<_>, _>>()?;
    let query = SourceSimilarityQuery::new(
        revision,
        anchor,
        request.aspect,
        request.generation,
        &candidates,
    )
    .map_err(|_| SimilarityDispatchFailure::InvalidInput)?;
    query_source_similarity(writer, &query, &cancellation.0).map_err(|error| match error {
        SimilarityQueryError::InvalidInput => SimilarityDispatchFailure::InvalidInput,
        SimilarityQueryError::Stale => SimilarityDispatchFailure::StaleSource,
        SimilarityQueryError::Cancelled => SimilarityDispatchFailure::Cancelled,
        SimilarityQueryError::AnchorUnavailable => SimilarityDispatchFailure::AnchorUnavailable,
        SimilarityQueryError::Observation(_) => SimilarityDispatchFailure::IdentityUnavailable,
        SimilarityQueryError::Owner(_) => SimilarityDispatchFailure::OwnerUnavailable,
    })
}

pub(super) fn dispatch(
    request: SourceSimilarityRequest,
    cancellation: SimilarityCancellation,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    if cancellation.is_cancelled() {
        return;
    }
    // Bind the same token to lifecycle retirement before touching retained owner data.
    let mut state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutdown || state.closed {
        return;
    }
    let active = state
        .sources
        .get(&request.source_id)
        .is_some_and(|live| live.active && live.lifecycle_generation == request.lifecycle);
    if !active {
        drop(state);
        reservation.publish(CatalogEvent::SimilarityCompleted {
            request,
            cancellation,
            result: Err(SimilarityDispatchFailure::StaleSource),
        });
        return;
    }
    let live = state
        .sources
        .get_mut(&request.source_id)
        .expect("checked source");
    if let Some(previous) = live.attempt_token.replace(Arc::clone(&cancellation.0)) {
        previous.store(true, Ordering::Release);
    }
    drop(state);
    let Some(source) = sources.get_mut(&request.source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&cancellation.0));
    let result = execute(&mut source.database.writer, &request, &cancellation);
    // Query validated its full observations after scoring. Publication validates the
    // retained source again; cancellation is also checked when delivery is consumed.
    if cancellation.is_cancelled() {
        return;
    }
    if result.is_err() {
        // A bounded diagnostic is not a source projection or read qualification.
        // Keep it available for the exact live caller even when physical custody
        // has expired; otherwise an accepted read would remain pending forever.
        let state = admission.lock().unwrap_or_else(|p| p.into_inner());
        if publication_is_current_state(
            &state,
            &request.source_id,
            request.lifecycle,
            Some(&cancellation.0),
        ) {
            reservation.publish(CatalogEvent::SimilarityCompleted {
                request,
                cancellation,
                result,
            });
        }
        return;
    }
    publish_source_if_current(
        admission,
        reservation,
        &request.source_id,
        request.lifecycle,
        Some(&cancellation.0),
        source,
        CatalogEvent::SimilarityCompleted {
            request: request.clone(),
            cancellation: cancellation.clone(),
            result,
        },
    );
}
