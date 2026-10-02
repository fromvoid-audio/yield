use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use yield_library::sample_sources::{
    GlobalLibraryWriterQueueError, GlobalLibraryWriterStatus, LibraryError,
    ProfileTagVocabularyClient,
};

pub use yield_library::sample_sources::library::ProfileAcquisitionId;
use yield_library::sample_sources::library::{
    GlobalMutationCompletion, GlobalMutationContext, GlobalMutationRequest, GlobalMutationValue,
};

pub use yield_library::sample_sources::{
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyEntry,
    ProfileTagVocabularyInputError, ProfileTagVocabularyLookup, ProfileTagVocabularyPage,
    ProfileTagVocabularyPageCursor, ProfileTagVocabularySetReceipt,
};

/// Maximum number of admitted profile vocabulary requests that may await terminal consumption.
pub const MAX_VOCABULARY_REQUESTS_IN_FLIGHT: usize = 8;

/// An id assigned to a profile vocabulary request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VocabularyRequestId(u64);

impl VocabularyRequestId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the numeric request id for logging or UI correlation.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Lifecycle state of the session-facing profile vocabulary slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VocabularyStatus {
    /// The profile owner is still initializing its database connection.
    Initializing,
    /// Vocabulary command admission is available.
    Available,
    /// Profile vocabulary admission failed while the session remains alive.
    Unavailable {
        /// Renderer-neutral failure description.
        reason: String,
    },
    /// Vocabulary command admission has been closed.
    Closed,
}

/// Errors returned before a profile vocabulary request reaches the library owner.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum VocabularySubmitError {
    /// The profile owner is still initializing its database connection.
    #[error("profile tag vocabulary is still initializing")]
    Initializing,
    /// The session's fixed outstanding-result budget is full.
    #[error("profile tag vocabulary request admission is full")]
    Full,
    /// A bounded vocabulary input was rejected before command admission.
    #[error("invalid profile tag vocabulary input: {0}")]
    Invalid(ProfileTagVocabularyInputError),
    /// The profile owner failed closed and cannot admit vocabulary commands.
    #[error("profile tag vocabulary is unavailable: {0}")]
    Unavailable(String),
    /// The session or profile owner has closed vocabulary command admission.
    #[error("profile tag vocabulary command admission is closed")]
    Closed,
}

/// A typed asynchronous profile vocabulary result.
///
/// The result receiver is single-consumer. An admitted request continues in the profile owner
/// after this handle is dropped; dropping the handle only releases the session's outstanding
/// result permit.
pub struct VocabularyRequest<T> {
    id: VocabularyRequestId,
    receiver: VocabularyReceiver<T>,
    acquisition: ProfileAcquisitionId,
    permit: Option<OutstandingVocabularyPermit>,
}

enum VocabularyReceiver<T> {
    Read(Receiver<Result<T, LibraryError>>),
    Mutation {
        receiver: Receiver<GlobalMutationCompletion>,
        context: GlobalMutationContext,
        project: fn(GlobalMutationValue) -> Result<T, LibraryError>,
    },
}

impl<T> VocabularyRequest<T> {
    /// Acquisition captured for this read or immutable mutation proposal; not authority.
    pub fn acquisition_id(&self) -> &ProfileAcquisitionId {
        &self.acquisition
    }

    /// Exact proposal context retained for mutation completion checking.
    pub fn mutation_context(&self) -> Option<&GlobalMutationContext> {
        match &self.receiver {
            VocabularyReceiver::Read(_) => None,
            VocabularyReceiver::Mutation { context, .. } => Some(context),
        }
    }

    /// Return the request id assigned at admission.
    pub fn id(&self) -> VocabularyRequestId {
        self.id
    }

    #[cfg(test)]
    pub(crate) fn inject_wrong_context_for_test(&mut self, replacement: GlobalMutationContext) {
        let VocabularyReceiver::Mutation { context, .. } = &mut self.receiver else {
            panic!("mutation request fixture");
        };
        *context = replacement;
    }

    /// Poll the request result without blocking the caller.
    ///
    /// The outstanding-result permit is released once a terminal result or disconnection is
    /// observed. An empty poll retains the permit for a later poll.
    pub fn try_recv(&mut self) -> Result<Result<T, LibraryError>, TryRecvError> {
        let received = match &self.receiver {
            VocabularyReceiver::Read(receiver) => receiver.try_recv(),
            VocabularyReceiver::Mutation {
                receiver,
                context,
                project,
            } => receiver
                .try_recv()
                .map(|completion| completion.into_result_for(context).and_then(project)),
        };
        match received {
            Ok(result) => {
                self.permit.take();
                Ok(result)
            }
            Err(TryRecvError::Empty) => Err(TryRecvError::Empty),
            Err(TryRecvError::Disconnected) => {
                self.permit.take();
                Err(TryRecvError::Disconnected)
            }
        }
    }
}

pub(crate) struct VocabularySlot {
    state: VocabularySlotState,
    client: Option<ProfileTagVocabularyClient>,
    outstanding: usize,
}

enum VocabularySlotState {
    Loading,
    Unavailable(String),
    Closed,
}

impl VocabularySlot {
    pub(crate) fn loading() -> Self {
        Self {
            state: VocabularySlotState::Loading,
            client: None,
            outstanding: 0,
        }
    }
}

pub(crate) fn status(slot: &Arc<Mutex<VocabularySlot>>) -> VocabularyStatus {
    let slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slot_status(&slot)
}

pub(crate) fn install(
    slot: &Arc<Mutex<VocabularySlot>>,
    client: ProfileTagVocabularyClient,
) -> bool {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if matches!(slot.state, VocabularySlotState::Closed) {
        return false;
    }
    slot.client = Some(client);
    true
}

pub(crate) fn mark_unavailable(slot: &Arc<Mutex<VocabularySlot>>, reason: String) {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !matches!(slot.state, VocabularySlotState::Closed) {
        slot.state = VocabularySlotState::Unavailable(reason);
        slot.client = None;
    }
}

pub(crate) fn close(slot: &Arc<Mutex<VocabularySlot>>) {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.state = VocabularySlotState::Closed;
}

pub(crate) fn lookup(
    slot: &Arc<Mutex<VocabularySlot>>,
    id: VocabularyRequestId,
    labels: Vec<String>,
) -> Result<VocabularyRequest<ProfileTagVocabularyLookup>, VocabularySubmitError> {
    submit(slot, id, move |client| {
        client
            .lookup_profile_tag_categories(labels)
            .map(VocabularyReceiver::Read)
    })
}

pub(crate) fn page(
    slot: &Arc<Mutex<VocabularySlot>>,
    id: VocabularyRequestId,
    expected_revision: Option<u64>,
    prefix: String,
    after_cursor: Option<ProfileTagVocabularyPageCursor>,
    requested_limit: usize,
) -> Result<VocabularyRequest<ProfileTagVocabularyPage>, VocabularySubmitError> {
    submit(slot, id, move |client| {
        client
            .profile_tag_vocabulary_page(expected_revision, prefix, after_cursor, requested_limit)
            .map(VocabularyReceiver::Read)
    })
}

pub(crate) fn set_category(
    slot: &Arc<Mutex<VocabularySlot>>,
    id: VocabularyRequestId,
    acquisition: Option<ProfileAcquisitionId>,
    expected_revision: u64,
    label: String,
    category: ProfileTagCategory,
) -> Result<VocabularyRequest<ProfileTagVocabularySetReceipt>, VocabularySubmitError> {
    submit(slot, id, move |client| {
        let request = GlobalMutationRequest::vocabulary_set(
            acquisition.unwrap_or_else(|| client.acquisition_id()),
            id.get(),
            expected_revision,
            label,
            category,
        )?;
        let context = request.context().clone();
        client
            .submit_captured_mutation(request)
            .map(|receiver| VocabularyReceiver::Mutation {
                receiver,
                context,
                project: |value| match value {
                    GlobalMutationValue::VocabularySet(receipt) => Ok(receipt),
                    _ => Err(LibraryError::GlobalMutationCompletionMismatch),
                },
            })
    })
}

pub(crate) fn delete_profile_tag_vocabulary(
    slot: &Arc<Mutex<VocabularySlot>>,
    id: VocabularyRequestId,
    acquisition: Option<ProfileAcquisitionId>,
    expected_revision: u64,
    label: String,
) -> Result<VocabularyRequest<ProfileTagVocabularyDeleteReceipt>, VocabularySubmitError> {
    submit(slot, id, move |client| {
        let request = GlobalMutationRequest::vocabulary_delete(
            acquisition.unwrap_or_else(|| client.acquisition_id()),
            id.get(),
            expected_revision,
            label,
        )?;
        let context = request.context().clone();
        client
            .submit_captured_mutation(request)
            .map(|receiver| VocabularyReceiver::Mutation {
                receiver,
                context,
                project: |value| match value {
                    GlobalMutationValue::VocabularyDelete(receipt) => Ok(receipt),
                    _ => Err(LibraryError::GlobalMutationCompletionMismatch),
                },
            })
    })
}

fn submit<T>(
    slot: &Arc<Mutex<VocabularySlot>>,
    id: VocabularyRequestId,
    operation: impl FnOnce(
        &ProfileTagVocabularyClient,
    ) -> Result<VocabularyReceiver<T>, GlobalLibraryWriterQueueError>,
) -> Result<VocabularyRequest<T>, VocabularySubmitError> {
    // Keep this lock through the bounded, nonblocking library admission. A successful request
    // therefore linearizes before session shutdown closes the slot, while no SQL or owner join
    // occurs under the lock.
    let mut slot_guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match slot_status(&slot_guard) {
        VocabularyStatus::Initializing => return Err(VocabularySubmitError::Initializing),
        VocabularyStatus::Unavailable { reason } => {
            return Err(VocabularySubmitError::Unavailable(reason));
        }
        VocabularyStatus::Closed => return Err(VocabularySubmitError::Closed),
        VocabularyStatus::Available => {}
    }
    if slot_guard.outstanding >= MAX_VOCABULARY_REQUESTS_IN_FLIGHT {
        return Err(VocabularySubmitError::Full);
    }
    let Some(client) = slot_guard.client.as_ref().cloned() else {
        return Err(VocabularySubmitError::Initializing);
    };
    slot_guard.outstanding += 1;
    let receiver = match operation(&client) {
        Ok(receiver) => receiver,
        Err(error) => {
            slot_guard.outstanding -= 1;
            return Err(map_submit_error(error));
        }
    };
    let acquisition = match &receiver {
        VocabularyReceiver::Read(_) => client.acquisition_id(),
        VocabularyReceiver::Mutation { context, .. } => context.acquisition().clone(),
    };
    Ok(VocabularyRequest {
        id,
        receiver,
        acquisition,
        permit: Some(OutstandingVocabularyPermit {
            slot: Arc::clone(slot),
        }),
    })
}

fn slot_status(slot: &VocabularySlot) -> VocabularyStatus {
    match &slot.state {
        VocabularySlotState::Loading => match slot.client.as_ref().map(|client| client.status()) {
            None | Some(GlobalLibraryWriterStatus::Initializing) => VocabularyStatus::Initializing,
            Some(GlobalLibraryWriterStatus::Available) => VocabularyStatus::Available,
            Some(GlobalLibraryWriterStatus::Unavailable { reason }) => {
                VocabularyStatus::Unavailable {
                    reason: reason.to_string(),
                }
            }
            Some(GlobalLibraryWriterStatus::Closing | GlobalLibraryWriterStatus::Closed) => {
                VocabularyStatus::Closed
            }
        },
        VocabularySlotState::Unavailable(reason) => VocabularyStatus::Unavailable {
            reason: reason.clone(),
        },
        VocabularySlotState::Closed => VocabularyStatus::Closed,
    }
}

fn map_submit_error(error: GlobalLibraryWriterQueueError) -> VocabularySubmitError {
    match error {
        GlobalLibraryWriterQueueError::Initializing => VocabularySubmitError::Initializing,
        GlobalLibraryWriterQueueError::Full => VocabularySubmitError::Full,
        GlobalLibraryWriterQueueError::Unavailable(reason) => {
            VocabularySubmitError::Unavailable(reason.to_string())
        }
        GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(error) => {
            VocabularySubmitError::Invalid(error)
        }
        GlobalLibraryWriterQueueError::Closed => VocabularySubmitError::Closed,
        GlobalLibraryWriterQueueError::InvalidMutationRoute
        | GlobalLibraryWriterQueueError::PayloadTooLarge { .. }
        | GlobalLibraryWriterQueueError::SourceRegistryTooManyDescriptors { .. }
        | GlobalLibraryWriterQueueError::SourceRegistryFieldTooLarge { .. }
        | GlobalLibraryWriterQueueError::SourceRegistrySourceIdTooLarge { .. }
        | GlobalLibraryWriterQueueError::SourceRegistryPayloadTooLarge { .. }
        | GlobalLibraryWriterQueueError::SourceRegistryPathTooLarge { .. } => {
            VocabularySubmitError::Unavailable(error.to_string())
        }
    }
}

struct OutstandingVocabularyPermit {
    slot: Arc<Mutex<VocabularySlot>>,
}

impl Drop for OutstandingVocabularyPermit {
    fn drop(&mut self) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.outstanding = slot.outstanding.saturating_sub(1);
    }
}

pub(crate) fn acquisition_id(
    slot: &Arc<Mutex<VocabularySlot>>,
) -> Result<ProfileAcquisitionId, VocabularySubmitError> {
    let slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match slot_status(&slot) {
        VocabularyStatus::Available => slot
            .client
            .as_ref()
            .map(ProfileTagVocabularyClient::acquisition_id)
            .ok_or(VocabularySubmitError::Initializing),
        VocabularyStatus::Initializing => Err(VocabularySubmitError::Initializing),
        VocabularyStatus::Unavailable { reason } => Err(VocabularySubmitError::Unavailable(reason)),
        VocabularyStatus::Closed => Err(VocabularySubmitError::Closed),
    }
}
