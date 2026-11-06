//! Bounded authored tempo reads and writes on the original Catalog owner.

use super::*;
pub use yield_library::sample_sources::SourcePlaymarkGridTempo;
pub use yield_library::sample_sources::db::SourcePlaymarkTempo;
use yield_library::sample_sources::{SourceRevision, readiness::SourceFeatureInputError};

/// Read or replace the authored span.
pub type PlaymarkTempoOperation = PlaymarkValueOperation<SourcePlaymarkTempo>;
/// Read or replace the explicit grid tempo.
pub type PlaymarkGridTempoOperation = PlaymarkValueOperation<SourcePlaymarkGridTempo>;
/// Exact audited Source request for the authored span.
pub type SourcePlaymarkTempoRequest = SourcePlaymarkValueRequest<SourcePlaymarkTempo>;
/// Exact audited Source request for the grid tempo.
pub type SourcePlaymarkGridTempoRequest = SourcePlaymarkValueRequest<SourcePlaymarkGridTempo>;

/// Read the current record or compare and replace the exact prior authored value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaymarkValueOperation<T> {
    /// Read without modifying source metadata.
    Read,
    /// Atomically change the tempo metadata, with `None` representing an absent record.
    Replace {
        /// Exact expected prior value.
        previous: Option<T>,
        /// Replacement value, or `None` to clear the record.
        next: Option<T>,
    },
}

/// One request scoped to the current audited file and its Catalog lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePlaymarkValueRequest<T> {
    source_id: SourceId,
    lifecycle_generation: u64,
    revision: u64,
    request_id: u64,
    path: RootRelativePath,
    operation: PlaymarkValueOperation<T>,
}

impl<T: Copy> SourcePlaymarkValueRequest<T> {
    /// Capture a bounded exact Source request and caller correlation ID.
    pub fn new(
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
        relative_path: &Path,
        operation: PlaymarkValueOperation<T>,
    ) -> Result<Self, PlaymarkTempoFailure> {
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || lifecycle_generation == 0
            || request_id == 0
            || relative_path.to_str().is_none_or(|path| path.len() > 1024)
        {
            return Err(PlaymarkTempoFailure::InvalidInput);
        }
        let path = RootRelativePath::try_from_path(relative_path.to_path_buf())
            .map_err(|_| PlaymarkTempoFailure::InvalidInput)?;
        Ok(Self {
            source_id: SourceId::from_string(source_id.as_str()),
            lifecycle_generation,
            revision,
            request_id,
            path,
            operation,
        })
    }

    /// Source that owns the media and metadata.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Registration lifecycle captured by the caller.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Audited manifest revision captured by the caller.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Caller-supplied request ID.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Exact Source-relative file.
    pub fn relative_path(&self) -> &Path {
        self.path.as_path()
    }

    /// Requested read or compare-and-set operation.
    pub fn operation(&self) -> PlaymarkValueOperation<T> {
        self.operation
    }

    pub(super) fn input_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.source_id.as_str().len()
            + encoded_path_len(self.relative_path())
    }
}

/// Bounded failure at the Source authoring boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaymarkTempoFailure {
    /// Invalid request identity or relative path.
    InvalidInput,
    /// The exact audited Source revision or lifecycle is no longer current.
    StaleSource,
    /// Cancellation was observed before any commit.
    Cancelled,
    /// The previous tempo no longer matches the stored value.
    Conflict,
    /// The audited media could not be admitted or revalidated.
    Input(SourceFeatureInputError),
    /// The Source owner refused or failed to complete the metadata operation.
    Store,
}

impl From<SourceWriterError> for PlaymarkTempoFailure {
    fn from(error: SourceWriterError) -> Self {
        match error {
            SourceWriterError::StaleFence | SourceWriterError::StaleRevision { .. } => {
                Self::StaleSource
            }
            SourceWriterError::FeatureInput(SourceFeatureInputError::Cancelled) => Self::Cancelled,
            SourceWriterError::FeatureInput(error) => Self::Input(error),
            _ => Self::Store,
        }
    }
}

pub(super) trait PlaymarkValue: Copy {
    fn read(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
    ) -> Result<Option<Self>, SourceWriterError>;

    fn replace(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
        previous: Option<Self>,
        next: Option<Self>,
        cancellation: &AtomicBool,
    ) -> Result<bool, SourceWriterError>;

    fn receipt(
        request: SourcePlaymarkValueRequest<Self>,
        cancellation: Arc<AtomicBool>,
        publication: CatalogPublicationStatus,
        result: Result<Option<Self>, PlaymarkTempoFailure>,
    ) -> CatalogEvent;
}

impl PlaymarkValue for SourcePlaymarkTempo {
    fn read(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
    ) -> Result<Option<Self>, SourceWriterError> {
        writer.read_playmark_tempo(revision, path)
    }

    fn replace(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
        previous: Option<Self>,
        next: Option<Self>,
        cancellation: &AtomicBool,
    ) -> Result<bool, SourceWriterError> {
        writer.compare_and_set_playmark_tempo(revision, path, previous, next, cancellation)
    }

    fn receipt(
        request: SourcePlaymarkValueRequest<Self>,
        cancellation: Arc<AtomicBool>,
        publication: CatalogPublicationStatus,
        result: Result<Option<Self>, PlaymarkTempoFailure>,
    ) -> CatalogEvent {
        CatalogEvent::PlaymarkTempoCompleted {
            request,
            cancellation,
            publication,
            result,
        }
    }
}

impl PlaymarkValue for SourcePlaymarkGridTempo {
    fn read(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
    ) -> Result<Option<Self>, SourceWriterError> {
        writer.read_playmark_grid_tempo(revision, path)
    }

    fn replace(
        writer: &SourceWriter,
        revision: &SourceRevision,
        path: &RootRelativePath,
        previous: Option<Self>,
        next: Option<Self>,
        cancellation: &AtomicBool,
    ) -> Result<bool, SourceWriterError> {
        writer.compare_and_set_playmark_grid_tempo(revision, path, previous, next, cancellation)
    }

    fn receipt(
        request: SourcePlaymarkValueRequest<Self>,
        cancellation: Arc<AtomicBool>,
        publication: CatalogPublicationStatus,
        result: Result<Option<Self>, PlaymarkTempoFailure>,
    ) -> CatalogEvent {
        CatalogEvent::PlaymarkGridTempoCompleted {
            request,
            cancellation,
            publication,
            result,
        }
    }
}

pub(super) fn dispatch<T: PlaymarkValue>(
    request: SourcePlaymarkValueRequest<T>,
    cancellation: Arc<AtomicBool>,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &HashMap<SourceId, RegisteredSource>,
) {
    let source = sources.get(&request.source_id);
    let admitted = {
        let mut state = admission.lock().unwrap_or_else(|error| error.into_inner());
        if publication_is_current_state(
            &state,
            &request.source_id,
            request.lifecycle_generation,
            None,
        ) {
            state.active_playmark_tempo =
                Some((request.source_id.clone(), Arc::clone(&cancellation)));
            true
        } else {
            false
        }
    };
    let result = if admitted {
        let result = source
            .ok_or(PlaymarkTempoFailure::StaleSource)
            .and_then(|source| execute(&source.database.writer, &request, &cancellation));
        admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_playmark_tempo = None;
        result
    } else {
        Err(PlaymarkTempoFailure::StaleSource)
    };
    let source_id = request.source_id.clone();
    publish_source_receipt(
        admission,
        reservation,
        &source_id,
        request.lifecycle_generation,
        None,
        source,
        |publication| T::receipt(request, cancellation, publication, result),
    );
}

fn execute<T: PlaymarkValue>(
    writer: &SourceWriter,
    request: &SourcePlaymarkValueRequest<T>,
    cancellation: &AtomicBool,
) -> Result<Option<T>, PlaymarkTempoFailure> {
    if cancellation.load(Ordering::Acquire) {
        return Err(PlaymarkTempoFailure::Cancelled);
    }
    let revision = match request.operation {
        PlaymarkValueOperation::Read => writer.current_revision(),
        PlaymarkValueOperation::Replace { .. } => writer.current_revision_for_command(),
    }?;
    if revision.revision() != request.revision
        || revision.lifecycle_generation() != request.lifecycle_generation
        || revision.source_id() != &request.source_id
    {
        return Err(PlaymarkTempoFailure::StaleSource);
    }
    match request.operation {
        PlaymarkValueOperation::Read => Ok(T::read(writer, &revision, &request.path)?),
        PlaymarkValueOperation::Replace { previous, next } => {
            let committed = T::replace(
                writer,
                &revision,
                &request.path,
                previous,
                next,
                cancellation,
            )?;
            if !committed {
                return Err(PlaymarkTempoFailure::Conflict);
            }
            Ok(next)
        }
    }
}
