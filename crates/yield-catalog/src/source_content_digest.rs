//! Retained Catalog ownership of a deferred scanner digest.

use super::*;
use yield_library::sample_sources::SourceRevision;
use yield_library::sample_sources::readiness::PreparedSourceContentDigest;

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

pub(super) fn commit(
    scope: SourceContentDigestScope,
    transient_request_id: Option<u64>,
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
            transient_request_id,
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
