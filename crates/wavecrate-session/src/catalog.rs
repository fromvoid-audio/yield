use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};

use thiserror::Error;
pub(crate) use wavecrate_catalog::CatalogService;
use wavecrate_catalog::CatalogSubmitError as CatalogWorkerSubmitError;

pub use wavecrate_catalog::{
    CatalogCommand, CatalogCurationRequestError, CatalogDisplayLabelsRequestError, CatalogError,
    CatalogEvent, CatalogOperation, CatalogRatingDecayRequestError, CatalogTagFilterRequestError,
    CatalogTagPageQuery, CatalogTagPageRequestError, CatalogWavMetadataRequestError,
    MAX_CURATION_OPERATION_BATCHES, MAX_CURATION_OPERATION_PATH_BYTES,
    MAX_CURATION_OPERATION_RECEIPT_BYTES, MAX_CURATION_OPERATION_UPDATES, MAX_CURATION_UPDATES,
    MAX_TAG_FILTER_MASK_BYTES, MAX_TAG_FILTER_PAYLOAD_BYTES, MAX_TAG_PAGE_INPUT_BYTES,
    MAX_WAV_METADATA_REQUEST_PATHS, SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATHS,
    SOURCE_TAG_PAGE_MAX_PAGE_SIZE, SOURCE_TAG_PAGE_MAX_PATH_BYTES, SOURCE_TAG_PAGE_MAX_PATHS,
    SOURCE_TAG_PAGE_MAX_PREFIX_BYTES, SOURCE_TAG_PAGE_MAX_TAG_IDS, SourceCurationCommitReceipt,
    SourceCurationOperationFailure, SourceCurationOperationFence, SourceCurationOperationOutcome,
    SourceCurationUpdate, SourceDisplayLabelEntry, SourceTagPage, SourceTagPageCursor,
    SourceTagPageEntry, VerifiedSourceRoot, resolve_source_root,
};

/// Lifecycle state of the independently-owned source catalog worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogStatus {
    /// Catalog startup is still acquiring the session profile capability.
    Loading,
    /// Catalog commands and events are available.
    Ready,
    /// Catalog startup or operation failed while the session owner remains usable.
    Unavailable {
        /// Renderer-neutral failure description.
        reason: String,
    },
    /// Catalog command admission has been closed.
    Closed,
}

/// Errors returned while admitting a catalog command.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogSubmitError {
    /// Catalog startup has not completed.
    #[error("source catalog is still loading")]
    NotReady,
    /// Catalog startup failed and no catalog worker is available.
    #[error("source catalog is unavailable: {0}")]
    Unavailable(String),
    /// The catalog worker's bounded command or event admission is full.
    #[error("source catalog command admission is full")]
    Full,
    /// Catalog command admission has been closed.
    #[error("source catalog command admission is closed")]
    Closed,
    /// A lazy WAV metadata request failed input admission.
    #[error("invalid source catalog WAV metadata request: {0}")]
    InvalidWavMetadataRequest(CatalogWavMetadataRequestError),
    /// A lazy tag-page request failed bounded input admission.
    #[error("invalid source catalog tag-page request: {0}")]
    InvalidTagPageRequest(CatalogTagPageRequestError),
    /// A source tag-filter request failed bounded input admission.
    #[error("invalid source catalog tag-filter request: {0}")]
    InvalidTagFilterRequest(CatalogTagFilterRequestError),
    /// A display-label request failed bounded input admission.
    #[error("invalid source catalog display-label request: {0}")]
    InvalidDisplayLabelsRequest(CatalogDisplayLabelsRequestError),
    /// A durable source curation request failed bounded or lifecycle admission.
    #[error("invalid source catalog curation request: {0}")]
    InvalidCurationRequest(CatalogCurationRequestError),
    /// A rating-decay request failed input validation.
    #[error("invalid source catalog rating-decay request: {0}")]
    InvalidRatingDecayRequest(CatalogRatingDecayRequestError),
}

pub(crate) struct CatalogSlot {
    status: CatalogStatus,
    service: Option<Arc<CatalogService>>,
}

impl CatalogSlot {
    pub(crate) fn loading() -> Self {
        Self {
            status: CatalogStatus::Loading,
            service: None,
        }
    }
}

pub(crate) fn status(slot: &Arc<Mutex<CatalogSlot>>) -> CatalogStatus {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .status
        .clone()
}

pub(crate) fn submit(
    slot: &Arc<Mutex<CatalogSlot>>,
    command: CatalogCommand,
) -> Result<(), CatalogSubmitError> {
    // Keep the slot lock through the bounded, nonblocking worker admission. This makes a
    // successful submit linearize before shutdown can close the session-facing slot while never
    // holding the lock through catalog I/O or a worker join.
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match &slot.status {
        CatalogStatus::Loading => Err(CatalogSubmitError::NotReady),
        CatalogStatus::Unavailable { reason } => {
            Err(CatalogSubmitError::Unavailable(reason.clone()))
        }
        CatalogStatus::Closed => Err(CatalogSubmitError::Closed),
        CatalogStatus::Ready => {
            let result = slot
                .service
                .as_ref()
                .ok_or(CatalogSubmitError::Closed)
                .and_then(|service| service.submit(command).map_err(map_submit_error));
            if matches!(result, Err(CatalogSubmitError::Closed)) {
                slot.status = CatalogStatus::Closed;
            }
            result
        }
    }
}

pub(crate) fn try_recv_event(slot: &Arc<Mutex<CatalogSlot>>) -> Result<CatalogEvent, TryRecvError> {
    let service = {
        let slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match &slot.status {
            CatalogStatus::Loading | CatalogStatus::Unavailable { .. } => {
                return Err(TryRecvError::Empty);
            }
            CatalogStatus::Closed => slot.service.clone().ok_or(TryRecvError::Disconnected)?,
            CatalogStatus::Ready => slot.service.clone().ok_or(TryRecvError::Disconnected)?,
        }
    };
    match service.try_recv_event() {
        Ok(event) => Ok(event),
        Err(error @ TryRecvError::Disconnected) => {
            mark_closed(slot, &service);
            Err(error)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn close(slot: &Arc<Mutex<CatalogSlot>>) {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.status = CatalogStatus::Closed;
}

pub(crate) fn mark_unavailable(slot: &Arc<Mutex<CatalogSlot>>, reason: String) {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !matches!(slot.status, CatalogStatus::Closed) {
        slot.status = CatalogStatus::Unavailable { reason };
        slot.service = None;
    }
}

pub(crate) fn install(slot: &Arc<Mutex<CatalogSlot>>, service: Arc<CatalogService>) -> bool {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if matches!(slot.status, CatalogStatus::Closed) {
        return false;
    }
    slot.status = CatalogStatus::Ready;
    slot.service = Some(service);
    true
}

fn mark_closed(slot: &Arc<Mutex<CatalogSlot>>, service: &Arc<CatalogService>) {
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot
        .service
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, service))
    {
        slot.status = CatalogStatus::Closed;
        slot.service = None;
    }
}

fn map_submit_error(error: CatalogWorkerSubmitError) -> CatalogSubmitError {
    match error {
        CatalogWorkerSubmitError::Full => CatalogSubmitError::Full,
        CatalogWorkerSubmitError::Closed => CatalogSubmitError::Closed,
        CatalogWorkerSubmitError::InvalidWavMetadataRequest(error) => {
            CatalogSubmitError::InvalidWavMetadataRequest(error)
        }
        CatalogWorkerSubmitError::InvalidTagPageRequest(error) => {
            CatalogSubmitError::InvalidTagPageRequest(error)
        }
        CatalogWorkerSubmitError::InvalidDisplayLabelsRequest(error) => {
            CatalogSubmitError::InvalidDisplayLabelsRequest(error)
        }
        CatalogWorkerSubmitError::InvalidTagFilterRequest(error) => {
            CatalogSubmitError::InvalidTagFilterRequest(error)
        }
        CatalogWorkerSubmitError::InvalidRatingDecayRequest(error) => {
            CatalogSubmitError::InvalidRatingDecayRequest(error)
        }
        CatalogWorkerSubmitError::InvalidCurationRequest(error) => {
            CatalogSubmitError::InvalidCurationRequest(error)
        }
    }
}
