//! Bounded authored tempo reads and writes on the original Catalog owner.

use super::*;
pub use yield_library::sample_sources::db::SourcePlaymarkTempo;
use yield_library::sample_sources::readiness::SourceFeatureInputError;

/// Read the current record or compare and replace the exact prior authored value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaymarkTempoOperation {
    /// Read without modifying source metadata.
    Read,
    /// Atomically change the tempo tuple, with `None` representing an absent record.
    Replace {
        /// Exact expected prior value.
        previous: Option<SourcePlaymarkTempo>,
        /// Replacement value, or `None` to clear the record.
        next: Option<SourcePlaymarkTempo>,
    },
}

/// One request scoped to the current audited file and its Catalog lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePlaymarkTempoRequest {
    source_id: SourceId,
    lifecycle_generation: u64,
    revision: u64,
    request_id: u64,
    path: RootRelativePath,
    operation: PlaymarkTempoOperation,
}

impl SourcePlaymarkTempoRequest {
    /// Capture a bounded exact Source request and caller correlation ID.
    pub fn new(
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
        relative_path: &Path,
        operation: PlaymarkTempoOperation,
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
    pub fn operation(&self) -> PlaymarkTempoOperation {
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

pub(super) fn dispatch(
    request: SourcePlaymarkTempoRequest,
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
        |publication| CatalogEvent::PlaymarkTempoCompleted {
            request,
            cancellation: Arc::clone(&cancellation),
            publication,
            result,
        },
    );
}

fn execute(
    writer: &SourceWriter,
    request: &SourcePlaymarkTempoRequest,
    cancellation: &AtomicBool,
) -> Result<Option<SourcePlaymarkTempo>, PlaymarkTempoFailure> {
    if cancellation.load(Ordering::Acquire) {
        return Err(PlaymarkTempoFailure::Cancelled);
    }
    let revision = match request.operation {
        PlaymarkTempoOperation::Read => writer.current_revision(),
        PlaymarkTempoOperation::Replace { .. } => writer.current_revision_for_command(),
    }?;
    if revision.revision() != request.revision
        || revision.lifecycle_generation() != request.lifecycle_generation
        || revision.source_id() != &request.source_id
    {
        return Err(PlaymarkTempoFailure::StaleSource);
    }
    match request.operation {
        PlaymarkTempoOperation::Read => Ok(writer.read_playmark_tempo(&revision, &request.path)?),
        PlaymarkTempoOperation::Replace { previous, next } => {
            let committed = writer.compare_and_set_playmark_tempo(
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
