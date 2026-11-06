//! Retained Catalog ownership of a deferred scanner digest.

use super::*;
use yield_library::sample_sources::SourceRevision;
use yield_library::sample_sources::readiness::{
    PreparedSourceContentDigest, SourceFeatureInputError,
};

pub(super) const MAX_SOURCE_BYTES: usize = 256;
pub(super) const MAX_PATH_BYTES: usize = 1024;
pub(super) const EVENT_BYTES: usize =
    MAX_SOURCE_BYTES + 2 * MAX_PATH_BYTES + 512 + 64 + std::mem::size_of::<CatalogEvent>();

/// The audited manifest entry whose deferred digest was computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceContentDigestScope {
    /// Stable owning Source.
    pub source_id: SourceId,
    /// Original Catalog registration lifecycle.
    pub lifecycle_generation: u64,
    /// Authoritative revision before enrichment.
    pub previous_revision: u64,
    /// Exact Source-relative file.
    pub relative_path: RootRelativePath,
}

impl SourceContentDigestScope {
    pub(super) fn capture(
        source_id: &SourceId,
        lifecycle_generation: u64,
        previous_revision: u64,
        request_id: u64,
        relative_path: &Path,
    ) -> Option<Self> {
        let id = source_id.as_str();
        if id.is_empty()
            || id.len() > MAX_SOURCE_BYTES
            || lifecycle_generation == 0
            || request_id == 0
            || relative_path
                .to_str()
                .is_none_or(|path| path.len() > MAX_PATH_BYTES)
        {
            return None;
        }
        Some(Self {
            source_id: source_id.clone(),
            lifecycle_generation,
            previous_revision,
            relative_path: RootRelativePath::try_from_path(relative_path.to_path_buf()).ok()?,
        })
    }

    pub(super) fn input_bytes(&self) -> usize {
        self.source_id.as_str().len() + encoded_path_len(self.relative_path.as_path())
    }
}

/// A bounded request to acquire only the digest for an exact pending Source entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceContentDigestRequest {
    scope: SourceContentDigestScope,
    request_id: u64,
}

impl SourceContentDigestRequest {
    /// Capture and validate the exact audited input and caller correlation.
    pub fn new(
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
        relative_path: &Path,
    ) -> Result<Self, SourceContentDigestFailure> {
        let scope = SourceContentDigestScope::capture(
            source_id,
            lifecycle_generation,
            revision,
            request_id,
            relative_path,
        )
        .ok_or(SourceContentDigestFailure::InvalidInput)?;
        Ok(Self { scope, request_id })
    }

    /// The captured Source generation and file.
    pub fn scope(&self) -> &SourceContentDigestScope {
        &self.scope
    }

    /// Caller identity for suppressing late completion.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(super) fn input_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.scope.input_bytes()
    }
}

/// The caller that caused an audited content digest to be filled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceContentDigestOrigin {
    /// A transient analysis that will now run against the enriched revision.
    Transients(u64),
    /// A standalone digest request with no detector continuation.
    DigestOnly(u64),
}

/// Why the exact Source digest was not acquired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceContentDigestFailure {
    /// Invalid request identity or path.
    InvalidInput,
    /// The retained Source or pending revision no longer matches.
    StaleSource,
    /// The exact media could not be held, read or verified.
    Input(SourceFeatureInputError),
    /// The bounded audio compute slots are occupied.
    Busy,
    /// The background worker could not compute the digest.
    Computation,
}

pub(super) fn commit(
    scope: SourceContentDigestScope,
    origin: SourceContentDigestOrigin,
    proof: PreparedSourceContentDigest,
    cancellation: &AtomicBool,
    source: &mut RegisteredSource,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
) -> Result<SourceRevision, SourceWriterError> {
    if proof.relative_path() != &scope.relative_path {
        return Err(SourceWriterError::StaleFence);
    }
    let snapshot = source
        .snapshot
        .as_mut()
        .filter(|snapshot| {
            snapshot.source_id == scope.source_id
                && snapshot.committed_revision == scope.previous_revision
                && snapshot.lifecycle_generation == scope.lifecycle_generation
        })
        .ok_or(SourceWriterError::StaleFence)?;
    let index = snapshot
        .manifest
        .binary_search_by(|entry| {
            entry
                .relative_path
                .as_path()
                .cmp(scope.relative_path.as_path())
        })
        .map_err(|_| SourceWriterError::StaleFence)?;
    let (revision, entry) = source
        .database
        .writer
        .commit_pending_content_digest(proof, cancellation)?;
    snapshot.manifest[index] = entry.clone();
    snapshot.committed_revision = revision.revision();
    let mut digest_event = reservation.split_read_event(EVENT_BYTES);
    let source_id = scope.source_id.clone();
    let lifecycle_generation = scope.lifecycle_generation;
    let published = publish_source_if_current(
        admission,
        &mut digest_event,
        &source_id,
        lifecycle_generation,
        None,
        source,
        CatalogEvent::SourceContentDigestCommitted {
            scope,
            origin,
            committed_revision: revision.revision(),
            entry,
        },
    );
    digest_event.finish();
    if !published {
        return Err(SourceWriterError::StaleFence);
    }
    Ok(revision)
}
