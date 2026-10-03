//! Opt-in service-local fault fixtures; unavailable in default catalog/app builds.
//!
//! These hooks cause real worker failures. They issue no source ownership or successful receipt.
use crate::{AdmissionState, CatalogError, CatalogService, SourceId};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Maximum retained UTF-8 source identity bytes in the single service-local fault slot.
pub const MAX_COMPOUND_FAULT_SOURCE_BYTES: usize = 256;

/// Refusal to retain an exact one-shot compound batch fixture.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CompoundBatchFaultError {
    /// Invalid bounded source/request/lifecycle/batch context.
    #[error("invalid compound batch fault context")]
    InvalidContext,
    /// The exact lifecycle is not currently admitted as active.
    #[error("compound batch fault lifecycle is unavailable")]
    StaleLifecycle,
    /// The service already retains a fault; no replacement or additional allocation occurs.
    #[error("compound batch fault slot is occupied")]
    Occupied,
    /// The service is shutting down or closed.
    #[error("compound batch fault service is closed")]
    Closed,
}

pub(crate) struct CompoundBatchFault {
    source: SourceId,
    lifecycle: u64,
    request: u64,
    batch: usize,
}
impl CatalogService {
    /// Arm one exact failure after existing worker fences and before the selected SQL batch.
    ///
    /// Other requests/sources/lifecycles cannot consume it. A matching terminal request,
    /// source replacement/removal, cancellation or service shutdown releases the slot.
    /// No database is opened and no production admission or receipt is bypassed.
    pub fn arm_compound_batch_failure(
        &self,
        source: &SourceId,
        lifecycle: u64,
        request: u64,
        batch: usize,
    ) -> Result<(), CompoundBatchFaultError> {
        use CompoundBatchFaultError as E;
        let id = source.as_str();
        if id.trim().is_empty()
            || id.contains('\0')
            || id.len() > MAX_COMPOUND_FAULT_SOURCE_BYTES
            || lifecycle == 0
            || request == 0
            || batch >= crate::MAX_CURATION_OPERATION_BATCHES
        {
            return Err(E::InvalidContext);
        }
        let mut state = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.closed {
            return Err(E::Closed);
        }
        if !state
            .sources
            .get(source)
            .is_some_and(|s| s.active && s.lifecycle_generation == lifecycle)
        {
            return Err(E::StaleLifecycle);
        }
        if state.compound_batch_fault.is_some() {
            return Err(E::Occupied);
        }
        state.compound_batch_fault = Some(CompoundBatchFault {
            source: source.clone(),
            lifecycle,
            request,
            batch,
        });
        Ok(())
    }

    /// Explicitly release this service's single pending fixture, returning whether one existed.
    pub fn cancel_compound_batch_failure(&self) -> bool {
        self.admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .compound_batch_fault
            .take()
            .is_some()
    }
}

pub(crate) fn clear_source(state: &mut AdmissionState, source: &SourceId) {
    if state
        .compound_batch_fault
        .as_ref()
        .is_some_and(|f| &f.source == source)
    {
        state.compound_batch_fault = None;
    }
}
pub(crate) fn finish_request(
    admission: &Arc<Mutex<AdmissionState>>,
    source: &SourceId,
    lifecycle: u64,
    request: u64,
) {
    let mut state = admission.lock().unwrap_or_else(|e| e.into_inner());
    if state
        .compound_batch_fault
        .as_ref()
        .is_some_and(|f| &f.source == source && f.lifecycle == lifecycle && f.request == request)
    {
        state.compound_batch_fault = None;
    }
}
pub(crate) fn take_failure(
    admission: &Arc<Mutex<AdmissionState>>,
    source: &SourceId,
    lifecycle: u64,
    request: u64,
    batch: usize,
) -> Result<(), CatalogError> {
    let mut state = admission.lock().unwrap_or_else(|e| e.into_inner());
    if state.compound_batch_fault.as_ref().is_some_and(|f| {
        &f.source == source && f.lifecycle == lifecycle && f.request == request && f.batch == batch
    }) {
        state.compound_batch_fault = None;
        return Err(CatalogError::Database(String::from(
            "test-support: exact compound batch failed before SQL",
        )));
    }
    Ok(())
}
