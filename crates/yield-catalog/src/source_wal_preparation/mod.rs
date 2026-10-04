//! Genuine catalog source actor adapter for the sealed library WAL transport.
use super::*;
use yield_library::sample_sources::SourceRevision;
pub use yield_library::sample_sources::wal_transport::{
    SOURCE_WAL_TRANSPORT_MAX_CONTEXT_BYTES as CATALOG_WAL_MAX_CONTEXT_BYTES,
    SOURCE_WAL_TRANSPORT_MAX_SLOTS as CATALOG_WAL_MAX_SLOTS, SourceWalTransport as Control,
    SourceWalTransportCorrelation as CatalogSourceWalCorrelation,
    SourceWalTransportObservation as CatalogSourceWalObservation,
    SourceWalTransportRejection as CatalogSourceWalRejection,
    SourceWalTransportRequest as CatalogSourceWalRequest,
    SourceWalTransportReservation as CatalogSourceWalPreparation,
    SourceWalTransportStatus as CatalogSourceWalStatus,
};
use yield_library::sample_sources::wal_transport::{
    SourceWalOwnerObservation, SourceWalTransportCommand, SourceWalTransportInbox,
    SourceWalTransportOwner,
};
mod owner;
pub(super) use owner::{WorkerReservations, deferred, interrupts_preparation, lifecycle};
impl CatalogService {
    /// Bounded library requester for the genuine catalog source producer.
    /// This handle carries no database owner or settlement authority.
    pub fn source_wal_transport(&self) -> Control {
        self.admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .source_wal
            .clone()
    }
    /// Queue strict original-reservation scope capture on the actual source worker.
    /// Requires that worker's already-retained genuine registration, not a caller phase/UUID.
    /// Complete shared scope bytes remain charged across copied diagnostics and abort.
    /// This pre-intent route cannot acknowledge settlement or issue physical/capacity permits.
    pub fn prepare_source_wal_with_binding(
        &self,
        expected: SourceRevision,
        correlation: CatalogSourceWalCorrelation,
        deadline: Instant,
    ) -> Result<CatalogSourceWalRequest, CatalogSourceWalRejection> {
        self.prepare_source_wal_scoped(expected, correlation, deadline, true)
    }
    /// Queue genuine retained-source pre-intent preparation on the original catalog worker.
    /// Expected revision is owner-produced and nonserializable. Correlations are observations;
    /// this operation never issues an ordinary ID, physical claim, journal latch or write grant.
    pub fn prepare_source_wal(
        &self,
        expected: SourceRevision,
        correlation: CatalogSourceWalCorrelation,
        deadline: Instant,
    ) -> Result<CatalogSourceWalRequest, CatalogSourceWalRejection> {
        self.prepare_source_wal_scoped(expected, correlation, deadline, false)
    }
    fn prepare_source_wal_scoped(
        &self,
        expected: SourceRevision,
        correlation: CatalogSourceWalCorrelation,
        deadline: Instant,
        require_binding: bool,
    ) -> Result<CatalogSourceWalRequest, CatalogSourceWalRejection> {
        let state = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutdown || state.closed {
            return Err(CatalogSourceWalRejection::Closed);
        }
        let desired = state
            .sources
            .get(expected.source_id())
            .ok_or(CatalogSourceWalRejection::Scope)?;
        if !desired.active || desired.lifecycle_generation != expected.lifecycle_generation() {
            return Err(CatalogSourceWalRejection::Scope);
        }
        if require_binding {
            state
                .source_wal
                .submit_with_binding(expected, correlation, deadline)
        } else {
            state.source_wal.submit(expected, correlation, deadline)
        }
    }
}

#[cfg(test)]
mod tests;
