//! Actual registered-owner inspection. This module never dispatches a retained pathname.
use super::*;
use yield_library::sample_sources::db::{SourceIndexEntryHandle, SourceIndexInspection};

/// Private owner/query/delivery/row binding, never constructed from caller observations.
#[derive(Clone, Debug)]
pub struct CatalogIndexEntryHandle {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
    pub(super) query_id: u64,
    pub(super) query_token: Arc<AtomicBool>,
    pub(super) entry: SourceIndexEntryHandle,
}
impl CatalogIndexEntryHandle {
    pub(super) fn input_bytes(&self) -> usize {
        self.source_id
            .as_str()
            .len()
            .saturating_add(self.entry.input_bytes())
    }
}

/// Caller-controlled denial only; cannot mint or authorize an inspection.
#[derive(Clone, Debug, Default)]
pub struct IndexInspectionCancellation(Arc<AtomicBool>);
impl IndexInspectionCancellation {
    /// Cancel pending work and queued/consumer-held delivery projection.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Cheap cancellation check.
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Private held object/root custody and owner query/request fences. Not native authority.
#[derive(Debug)]
pub struct CatalogIndexInspection {
    held: SourceIndexInspection,
    query_token: Arc<AtomicBool>,
    cancellation: IndexInspectionCancellation,
}
impl CatalogIndexInspection {
    /// Facts inspected by the real owner, without an absolute path or dispatch API.
    pub fn entry(&self) -> &yield_library::sample_sources::SourceIndexEntry {
        self.held.entry()
    }
    /// Independent retained index revision.
    pub fn index_revision(&self) -> u64 {
        self.held.index_revision()
    }
    /// Cheap source/query/request check only; not final filesystem/native admission.
    pub fn is_live(&self) -> bool {
        self.held.is_live()
            && !self.query_token.load(Ordering::Acquire)
            && !self.cancellation.is_canceled()
    }
}

pub(super) fn request(
    handle: CatalogIndexEntryHandle,
    request_id: u64,
    cancellation: IndexInspectionCancellation,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    if cancellation.is_canceled() || handle.query_token.load(Ordering::Acquire) {
        return;
    }
    let current = admission_state(admission, |state| {
        state
            .index_queries
            .get(&handle.source_id)
            .is_some_and(|(g, q, token)| {
                *g == handle.lifecycle_generation
                    && *q == handle.query_id
                    && Arc::ptr_eq(token, &handle.query_token)
            })
    });
    if !current {
        return;
    }
    let Some(source) = sources
        .get(&handle.source_id)
        .filter(|s| s.lifecycle_generation == handle.lifecycle_generation)
    else {
        return;
    };
    let result = if profile_guard.validate_current().is_err()
        || source
            .database
            .validate_profile_guard(profile_guard)
            .is_err()
        || source.publication_fence.validate().is_err()
    {
        Err(SourceIndexInspectionError::OwnerUnavailable)
    } else {
        source
            .database
            .inspect_index_entry(&handle.entry)
            .and_then(|held| {
                source.database.validate_index_inspection(&held)?;
                Ok(CatalogIndexInspection {
                    held,
                    query_token: handle.query_token.clone(),
                    cancellation: cancellation.clone(),
                })
            })
    };
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if !publication_is_current_state(&state, &handle.source_id, handle.lifecycle_generation, None)
        || handle.query_token.load(Ordering::Acquire)
        || cancellation.is_canceled()
    {
        return;
    }
    let event = CatalogEvent::IndexInspectionCompleted {
        source_id: handle.source_id,
        lifecycle_generation: handle.lifecycle_generation,
        query_id: handle.query_id,
        request_id,
        cancellation,
        result,
    };
    assert!(reservation.hub.send_index_page(
        event,
        source.publication_fence.clone(),
        handle.query_token,
        reservation.tag_read_payload_bytes
    ));
    reservation.remaining -= 1;
    reservation.tag_read_payload_bytes = 0;
}
