//! Actual retained-owner Harvest reads and cheap projection fences.
use super::*;
use yield_library::operation_journal::StoredRelativePath;
pub use yield_library::sample_sources::library::{
    HARVEST_FACTS_MAX_BYTES, HARVEST_FACTS_MAX_PATH_BYTES, HARVEST_FACTS_MAX_PATHS,
    HarvestFactsCancellation, HarvestFactsEntry, HarvestState,
};
use yield_library::sample_sources::library::{
    HarvestFactsBatch, HarvestFactsClient, ProfileAcquisitionId,
};
mod dispatch;
mod filter;
mod read;
pub(super) use dispatch::dispatch;
pub(super) use filter::dispatch as dispatch_filter;
pub use filter::{CatalogHarvestFilter, HarvestFilterChoice, SourceHarvestFilterRequest};

pub(super) const MAX_RESULT_BYTES: usize = HARVEST_FACTS_MAX_BYTES * 2
    + HARVEST_FACTS_MAX_PATH_BYTES
    + HARVEST_FACTS_MAX_PATHS * 128
    + 4096;

/// Immutable bounded query and exact consumer projection context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceHarvestFactsRequest {
    profile: ProfileAcquisitionId,
    source_id: SourceId,
    lifecycle: u64,
    root: RootIdentity,
    directory: u64,
    revision: u64,
    request_id: u64,
    paths: Vec<StoredRelativePath>,
}
impl SourceHarvestFactsRequest {
    /// Capture the actual session acquisition and exact current browser/catalog context.
    pub fn new(
        profile: ProfileAcquisitionId,
        source_id: SourceId,
        lifecycle: u64,
        root: RootIdentity,
        directory: u64,
        revision: u64,
        request_id: u64,
        paths: &[String],
    ) -> Result<Self, HarvestFactsFailure> {
        Self::context(
            profile.clone(),
            source_id.clone(),
            lifecycle,
            root.clone(),
            directory,
            revision,
            request_id,
        )?;
        if paths.is_empty() || paths.len() > HARVEST_FACTS_MAX_PATHS {
            return Err(HarvestFactsFailure::InvalidInput);
        }
        let mut bytes = 0usize;
        let mut seen = std::collections::BTreeSet::new();
        let mut captured = Vec::with_capacity(paths.len());
        for path in paths {
            bytes = bytes
                .checked_add(path.len())
                .ok_or(HarvestFactsFailure::InvalidInput)?;
            if bytes > HARVEST_FACTS_MAX_PATH_BYTES || !seen.insert(path) {
                return Err(HarvestFactsFailure::InvalidInput);
            }
            let key = StoredRelativePath::new(path.clone())
                .map_err(|_| HarvestFactsFailure::InvalidInput)?;
            // No native spelling may alias another manifest key through normalization.
            let normalized =
                yield_library::sample_sources::db::normalize_relative_path(Path::new(path))
                    .map_err(|_| HarvestFactsFailure::InvalidInput)?;
            if normalized != *path {
                return Err(HarvestFactsFailure::InvalidInput);
            }
            captured.push(key);
        }
        Ok(Self {
            profile,
            source_id,
            lifecycle,
            root,
            directory,
            revision,
            request_id,
            paths: captured,
        })
    }
    fn context(
        profile: ProfileAcquisitionId,
        source_id: SourceId,
        lifecycle: u64,
        root: RootIdentity,
        directory: u64,
        revision: u64,
        request_id: u64,
    ) -> Result<Self, HarvestFactsFailure> {
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || root.as_bytes().is_empty()
            || root.as_bytes().len() > 256
            || lifecycle == 0
            || directory == 0
            || request_id == 0
        {
            return Err(HarvestFactsFailure::InvalidInput);
        }
        Ok(Self {
            profile,
            source_id,
            lifecycle,
            root,
            directory,
            revision,
            request_id,
            paths: Vec::new(),
        })
    }
    /// Actual captured profile acquisition; diagnostic context, not a write grant.
    pub fn profile_acquisition(&self) -> &ProfileAcquisitionId {
        &self.profile
    }
    /// Captured source identifier.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    /// Captured source lifecycle.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle
    }
    /// Captured physical source-root identity.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root
    }
    /// Captured authoritative directory generation.
    pub fn directory_generation(&self) -> u64 {
        self.directory
    }
    /// Captured committed catalog revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Caller-assigned nonzero request identifier.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }
    /// Exact unique paths, in caller order.
    pub fn paths(&self) -> &[StoredRelativePath] {
        &self.paths
    }
    /// Cheap exact context comparison; performs no SQL, filesystem or locking work.
    /// The consumer must also match its pending request ID and selected path set.
    pub fn matches_projection(
        &self,
        profile: &ProfileAcquisitionId,
        source_id: &SourceId,
        lifecycle: u64,
        root: &RootIdentity,
        directory: u64,
        revision: u64,
    ) -> bool {
        &self.profile == profile
            && &self.source_id == source_id
            && self.lifecycle == lifecycle
            && &self.root == root
            && self.directory == directory
            && self.revision == revision
    }
    pub(super) fn input_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.source_id.as_str().len()
            + self.root.as_bytes().len()
            + self.profile.as_str().len()
            + self
                .paths
                .iter()
                .map(|p| p.as_str().len() + std::mem::size_of::<StoredRelativePath>())
                .sum::<usize>()
    }
}
/// Bounded failure vocabulary; never an optimistic state/count or physical qualification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarvestFactsFailure {
    /// Empty, duplicate, unsafe or over-budget input.
    InvalidInput,
    /// Source/root/lifecycle/directory/audited revision changed.
    StaleSource,
    /// Exact audio metadata/content identity is unavailable or uncertain.
    IdentityUnavailable,
    /// The actual receiving profile/global owner is absent, changed, closed or busy.
    OwnerUnavailable,
    /// Stored Harvest metadata is malformed or exceeds its bounded graph shape.
    Malformed,
    /// Bounded SQLite work or end-to-end owner wait expired.
    QueryBudget,
    /// An actual Harvest mutation invalidated the coherent read generation.
    GlobalChanged,
    /// Explicit cancellation or supersession ended the request.
    Canceled,
}
/// Privately produced facts retaining actual source, attempt and global publication liveness.
/// Retention holds no source/global worker, writable connection or profile lock alive.
#[derive(Clone, Debug)]
pub struct CatalogHarvestFacts {
    request: SourceHarvestFactsRequest,
    batch: HarvestFactsBatch,
    source: SourcePublicationFence,
    attempt: Arc<AtomicBool>,
    cancellation: HarvestFactsCancellation,
}
impl CatalogHarvestFacts {
    /// Exact captured request context and path order.
    pub fn request(&self) -> &SourceHarvestFactsRequest {
        &self.request
    }
    /// Exact bounded historical state/count pairs, in requested path order.
    pub fn entries(&self) -> &[HarvestFactsEntry] {
        self.batch.entries()
    }
    /// Cheap retained source/attempt/global/cancellation check, with no SQL or filesystem work.
    /// A current consumer projection context must additionally match before every application.
    pub fn is_live(&self) -> bool {
        !self.cancellation.is_canceled()
            && !self.attempt.load(Ordering::Acquire)
            && self.source.is_live()
            && self.batch.is_current_global_generation()
    }
    /// Check liveness and the consumer's exact current projection using only in-memory values.
    /// Callers must also match their pending request ID/path set; no cross-owner atomicity is implied.
    pub fn matches_projection(
        &self,
        profile: &ProfileAcquisitionId,
        source_id: &SourceId,
        lifecycle: u64,
        root: &RootIdentity,
        directory: u64,
        revision: u64,
    ) -> bool {
        self.is_live()
            && self
                .request
                .matches_projection(profile, source_id, lifecycle, root, directory, revision)
    }
}
