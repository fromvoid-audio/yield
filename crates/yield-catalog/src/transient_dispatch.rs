//! One bounded transient request on the retained Catalog background worker.

use super::*;
use yield_analysis::transients::{MAX_TRANSIENT_MARKERS, TransientPreset};
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
    /// The bounded compute slot and its waiting request are occupied.
    Busy,
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

pub(super) struct Submission<'a> {
    pub(super) request: SourceTransientRequest,
    pub(super) cancellation: FeatureAnalysisCancellation,
    pub(super) reservation: EventReservation<'a>,
}

struct Running<'a> {
    submission: Submission<'a>,
    compute: std::thread::JoinHandle<ComputedTransientJob>,
}

/// One compute thread and one waiting request per original Catalog worker.
#[derive(Default)]
pub(super) struct Worker<'a> {
    running: Option<Running<'a>>,
    waiting: Option<Submission<'a>>,
}

impl<'a> Worker<'a> {
    pub(super) fn submit(
        &mut self,
        submission: Submission<'a>,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &HashMap<SourceId, RegisteredSource>,
        test_gate: Option<&Arc<CatalogTestGate>>,
    ) {
        if submission.cancellation.is_cancelled() {
            submission.reservation.finish();
            return;
        }
        if self.running.is_none() && self.waiting.is_none() {
            self.start(submission, admission, sources, test_gate);
            return;
        }
        if self
            .waiting
            .as_ref()
            .is_some_and(|pending| pending.cancellation.is_cancelled())
            && let Some(old) = self.waiting.take()
        {
            old.reservation.finish();
        }
        if self.waiting.is_none() {
            self.waiting = Some(submission);
        } else {
            submission.complete(Err(TransientRequestFailure::Busy), admission, None);
        }
    }

    pub(super) fn poll(
        &mut self,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &HashMap<SourceId, RegisteredSource>,
        test_gate: Option<&Arc<CatalogTestGate>>,
    ) {
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.compute.is_finished())
            && let Some(Running {
                submission,
                compute,
            }) = self.running.take()
        {
            let source = sources.get(submission.request.source_id());
            let result = if submission.cancellation.is_cancelled() {
                Err(TransientRequestFailure::Cancelled)
            } else {
                compute
                    .join()
                    .map_err(|_| TransientRequestFailure::Computation)
                    .and_then(|computed| {
                        let source = source.ok_or(TransientRequestFailure::StaleSource)?;
                        computed.finish(&source.database.writer).map_err(Into::into)
                    })
            };
            clear_active(admission);
            submission.complete(result, admission, source);
        }
        if self.running.is_none()
            && let Some(waiting) = self.waiting.take()
        {
            self.start(waiting, admission, sources, test_gate);
        }
    }

    fn start(
        &mut self,
        submission: Submission<'a>,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &HashMap<SourceId, RegisteredSource>,
        _test_gate: Option<&Arc<CatalogTestGate>>,
    ) {
        {
            let mut state = admission.lock().unwrap_or_else(|error| error.into_inner());
            if submission.cancellation.is_cancelled()
                || !publication_is_current_state(
                    &state,
                    submission.request.source_id(),
                    submission.request.lifecycle_generation(),
                    None,
                )
            {
                drop(state);
                submission.reservation.finish();
                return;
            }
            state.active_transient = Some((
                submission.request.source_id().clone(),
                submission.cancellation.clone(),
            ));
        }
        let source = sources.get(submission.request.source_id());
        let prepared = source
            .ok_or(TransientRequestFailure::StaleSource)
            .and_then(|source| {
                prepare(
                    &source.database.writer,
                    &submission.request,
                    &submission.cancellation,
                )
            });
        let compute = prepared.and_then(|job| {
            #[cfg(test)]
            let gate = _test_gate.cloned();
            std::thread::Builder::new()
                .name("yield-transient-compute".into())
                .spawn(move || {
                    #[cfg(test)]
                    if let Some(gate) = gate {
                        gate.before_transient_compute();
                    }
                    job.compute()
                })
                .map_err(|_| TransientRequestFailure::Computation)
        });
        match compute {
            Ok(compute) => {
                self.running = Some(Running {
                    submission,
                    compute,
                })
            }
            Err(error) => {
                clear_active(admission);
                submission.complete(Err(error), admission, source);
            }
        }
    }
}

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        // Cancel before releasing the original owner. A worker in an OS read
        // cannot publish and does not own a writer or wait on Catalog shutdown.
        if let Some(Running { submission, .. }) = self.running.take() {
            submission.cancellation.cancel();
            submission.reservation.finish();
        }
        if let Some(submission) = self.waiting.take() {
            submission.cancellation.cancel();
            submission.reservation.finish();
        }
    }
}

fn clear_active(admission: &Arc<Mutex<AdmissionState>>) {
    admission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .active_transient = None;
}

impl Submission<'_> {
    fn complete(
        mut self,
        result: Result<TransientJobOutcome, TransientRequestFailure>,
        admission: &Arc<Mutex<AdmissionState>>,
        source: Option<&RegisteredSource>,
    ) {
        if self.cancellation.is_cancelled() {
            self.reservation.finish();
            return;
        }
        let event = |result| CatalogEvent::TransientMarkersCompleted {
            request: self.request.clone(),
            cancellation: self.cancellation.clone(),
            result,
        };
        let failure = match result {
            Ok(markers) => {
                if let Some(source) = source
                    && publish_source_if_current(
                        admission,
                        &mut self.reservation,
                        self.request.source_id(),
                        self.request.lifecycle_generation(),
                        None,
                        source,
                        event(Ok(markers)),
                    )
                {
                    self.reservation.finish();
                    return;
                }
                TransientRequestFailure::StaleSource
            }
            Err(error) => error,
        };
        publish_if_current(
            admission,
            &mut self.reservation,
            self.request.source_id(),
            self.request.lifecycle_generation(),
            None,
            event(Err(failure)),
        );
        self.reservation.finish();
    }
}

fn prepare(
    writer: &SourceWriter,
    request: &SourceTransientRequest,
    cancellation: &FeatureAnalysisCancellation,
) -> Result<PreparedTransientJob, TransientRequestFailure> {
    let revision = writer
        .current_revision()
        .map_err(|_| TransientRequestFailure::StaleSource)?;
    if revision.source_id() != request.source_id()
        || revision.lifecycle_generation() != request.lifecycle_generation()
        || revision.revision() != request.revision()
    {
        return Err(TransientRequestFailure::StaleSource);
    }
    Ok(prepare_transient_job(
        writer,
        &revision,
        &request.relative_path,
        request.preset(),
        cancellation.clone(),
    )?)
}
