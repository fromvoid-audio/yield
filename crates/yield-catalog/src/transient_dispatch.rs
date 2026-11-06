//! One bounded transient request on the retained Catalog background worker.

use super::*;
use yield_analysis::transients::{MAX_TRANSIENT_MARKERS, TransientMarkers, TransientPreset};
use yield_library::sample_sources::readiness::SourceFeatureInputError;

const MAX_SOURCE_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 1024;
pub(super) const MAX_RESULT_BYTES: usize = MAX_SOURCE_BYTES
    + MAX_PATH_BYTES
    + MAX_TRANSIENT_MARKERS * std::mem::size_of::<u32>()
    + std::mem::size_of::<CatalogEvent>();

/// Exact bounded source file and detector configuration requested by one caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceTransientRequest {
    source_id: SourceId,
    lifecycle_generation: u64,
    revision: u64,
    request_id: u64,
    relative_path: RootRelativePath,
    preset: TransientPreset,
}

impl SourceTransientRequest {
    /// Capture bounded request fields before queue admission.
    pub fn new(
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
        relative_path: &Path,
        preset: TransientPreset,
    ) -> Result<Self, TransientRequestFailure> {
        let id = source_id.as_str();
        if id.is_empty()
            || id.len() > MAX_SOURCE_BYTES
            || lifecycle_generation == 0
            || request_id == 0
            || relative_path
                .to_str()
                .is_none_or(|path| path.len() > MAX_PATH_BYTES)
        {
            return Err(TransientRequestFailure::InvalidInput);
        }
        let relative_path = RootRelativePath::try_from_path(relative_path.to_path_buf())
            .map_err(|_| TransientRequestFailure::InvalidInput)?;
        Ok(Self {
            source_id: SourceId::from_string(id),
            lifecycle_generation,
            revision,
            request_id,
            relative_path,
            preset,
        })
    }

    /// Exact registered source.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Registered lifecycle captured by the caller.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Audited manifest revision captured by the caller.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Caller-supplied identity for rejecting late completions.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Native path relative to the registered source.
    pub fn relative_path(&self) -> &Path {
        self.relative_path.as_path()
    }

    /// Detector preset captured by the request.
    pub fn preset(&self) -> TransientPreset {
        self.preset
    }

    pub(super) fn input_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.source_id.as_str().len()
            + encoded_path_len(self.relative_path())
    }
}

/// Bounded failure for one transient request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransientRequestFailure {
    /// Invalid or over-budget request fields.
    InvalidInput,
    /// The retained Source or audited revision did not match.
    StaleSource,
    /// The source media was missing, unsupported, changed or over budget.
    Input(SourceFeatureInputError),
    /// The WAV bytes could not be decoded under the bounded PCM contract.
    Decode(&'static str),
    /// The detector could not produce a result.
    Computation,
    /// The caller or Source lifecycle cancelled the request.
    Cancelled,
}

impl From<TransientJobError> for TransientRequestFailure {
    fn from(error: TransientJobError) -> Self {
        match error {
            TransientJobError::Input(error)
            | TransientJobError::Source(SourceWriterError::FeatureInput(error)) => {
                Self::Input(error)
            }
            TransientJobError::Source(_) => Self::StaleSource,
            TransientJobError::Decode(reason) => Self::Decode(reason),
            TransientJobError::Analysis(_) => Self::Computation,
            TransientJobError::Cancelled => Self::Cancelled,
        }
    }
}

pub(super) fn dispatch(
    request: SourceTransientRequest,
    cancellation: FeatureAnalysisCancellation,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &HashMap<SourceId, RegisteredSource>,
) {
    {
        let mut state = admission.lock().unwrap_or_else(|error| error.into_inner());
        if cancellation.is_cancelled()
            || !publication_is_current_state(
                &state,
                request.source_id(),
                request.lifecycle_generation(),
                None,
            )
        {
            return;
        }
        state.active_transient = Some((request.source_id().clone(), cancellation.clone()));
    }

    let source = sources.get(request.source_id());
    let result = source
        .ok_or(TransientRequestFailure::StaleSource)
        .and_then(|source| execute(&source.database.writer, &request, &cancellation));
    admission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .active_transient = None;

    if cancellation.is_cancelled() {
        return;
    }
    let event = |result| CatalogEvent::TransientMarkersCompleted {
        request: request.clone(),
        cancellation: cancellation.clone(),
        result,
    };
    let failure = match result {
        Ok(markers) => {
            if let Some(source) = source
                && publish_source_if_current(
                    admission,
                    reservation,
                    request.source_id(),
                    request.lifecycle_generation(),
                    None,
                    source,
                    event(Ok(markers)),
                )
            {
                return;
            }
            TransientRequestFailure::StaleSource
        }
        Err(error) => error,
    };
    publish_if_current(
        admission,
        reservation,
        request.source_id(),
        request.lifecycle_generation(),
        None,
        event(Err(failure)),
    );
}

fn execute(
    writer: &SourceWriter,
    request: &SourceTransientRequest,
    cancellation: &FeatureAnalysisCancellation,
) -> Result<TransientMarkers, TransientRequestFailure> {
    let revision = writer
        .current_revision()
        .map_err(|_| TransientRequestFailure::StaleSource)?;
    if revision.source_id() != request.source_id()
        || revision.lifecycle_generation() != request.lifecycle_generation()
        || revision.revision() != request.revision()
    {
        return Err(TransientRequestFailure::StaleSource);
    }
    // Catalog is already a background worker. The bounded compute phase holds
    // a read capability only and borrows neither the writer nor a SQL transaction.
    Ok(prepare_transient_job(
        writer,
        &revision,
        &request.relative_path,
        request.preset(),
        cancellation.clone(),
    )?
    .compute()
    .finish(writer)?)
}
