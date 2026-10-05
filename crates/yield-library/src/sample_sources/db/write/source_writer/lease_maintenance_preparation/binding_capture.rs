//! Original prepared owner issues/revalidates a bounded lease scope, never an admitted binding.
use super::{SourceLeaseMaintenancePreparation, SourceLeaseMaintenancePreparationError};
use crate::lifecycle_allocation::{self, LifecycleAllocationError};
use crate::operation_journal::source_lease_binding::StoredSourceLeaseFence;
use crate::sample_sources::db::{SourceAuthorityRegistration, source_registry_reference};
use std::{
    sync::{Arc, Weak},
    time::Instant,
};
use uuid::Uuid;

/// Genuine prepared-source binding capture, without a registration/capacity/commit grant.
/// Non-deserializable and tied to the original preparation/issuer; stored copies are observations.
#[derive(Debug)]
pub struct SourceLeaseMaintenanceBinding {
    stored: StoredSourceLeaseFence,
    issuer: Weak<()>,
    nonce: Uuid,
    deadline: Instant,
}
impl SourceLeaseMaintenanceBinding {
    pub(crate) fn retained_memory_bytes(&self) -> Option<usize> {
        std::mem::size_of::<Self>().checked_add(self.stored.retained_heap_bytes()?)
    }
    /// Borrow historical finite scope. Prepared/registered phase is not full journal readiness.
    pub fn stored_fence(&self) -> &StoredSourceLeaseFence {
        &self.stored
    }
}
impl SourceLeaseMaintenancePreparation {
    /// Capture the original registration/source tuple inside the retained empty MAIN transaction.
    /// A genuine Prepared registration can be observed; that is not admitted retirement authority.
    pub fn capture_lease_binding(
        &self,
        registration: &SourceAuthorityRegistration,
    ) -> Result<SourceLeaseMaintenanceBinding, SourceLeaseMaintenancePreparationError> {
        self.revalidate()?;
        let writer = self.custody.writer();
        let subject = lifecycle_allocation::bounded_read(
            &writer.database.connection,
            self.deadline(),
            || {
                let scan = writer
                    .current_scan_fence()
                    .map_err(|_| LifecycleAllocationError::Unknown)?;
                let actual = source_registry_reference::capture_registered_in_transaction(
                    &writer.database,
                    &scan,
                    registration,
                )
                .map_err(|_| LifecycleAllocationError::Unknown)?;
                Ok(actual.subject().clone())
            },
        )?;
        let stored = StoredSourceLeaseFence::from_capture(
            subject,
            self.observation().allocation(),
            self.nonce(),
        )?;
        self.revalidate()?;
        let after = registration
            .registry_reference_observation(&writer.database)
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        if stored.subject().registration() != Some(&after) {
            return Err(LifecycleAllocationError::Unknown.into());
        }
        if Instant::now() >= self.deadline() {
            return Err(SourceLeaseMaintenancePreparationError::Deadline);
        }
        Ok(SourceLeaseMaintenanceBinding {
            stored,
            issuer: Arc::downgrade(&writer.reference_liveness),
            nonce: self.nonce(),
            deadline: self.deadline(),
        })
    }
    /// Validate parked original custody and actual registration without ordinary SQL.
    /// Matching stored tuples, nonce strings or decoded observations cannot substitute custody.
    pub fn validate_lease_binding(
        &self,
        binding: &SourceLeaseMaintenanceBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceLeaseMaintenancePreparationError> {
        if binding.nonce != self.nonce()
            || binding.deadline != self.deadline()
            || !Weak::ptr_eq(
                &binding.issuer,
                &Arc::downgrade(&self.custody.writer().reference_liveness),
            )
        {
            return Err(LifecycleAllocationError::Unknown.into());
        }
        self.revalidate_owner()?;
        let current = registration
            .registry_reference_observation(&self.custody.writer().database)
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        if binding.stored.subject().registration() != Some(&current) {
            return Err(LifecycleAllocationError::Unknown.into());
        }
        self.revalidate_owner()?;
        Ok(())
    }
    /// Require genuine current Registered authority on the original pre-audit reservation.
    /// This validates CONTROL/native custody and entered policy without ordinary SQL;
    /// it neither renews the lease nor grants settlement, effect or capacity authority.
    pub fn validate_registered_lease_binding(
        &self,
        binding: &SourceLeaseMaintenanceBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceLeaseMaintenancePreparationError> {
        self.validate_lease_binding(binding, registration)?;
        if let Some(custody) = self.custody.registered.as_ref() {
            custody
                .validate(&self.custody.writer().database, registration)
                .map_err(|_| LifecycleAllocationError::Unknown)?;
        }
        registration
            .validate_registered_database_boundary(&self.custody.writer().database)
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        self.validate_lease_binding(binding, registration)
    }
}

impl SourceLeaseMaintenancePreparation {
    /// Retain one genuine Registered use on this exact original pre-audit reservation.
    /// Custody survives rollback, timeout and policy restoration failure until real close.
    /// No audit/revision, decision, write, capacity, deadline extension or settlement is issued.
    pub fn retain_registered_lease_binding(
        &mut self,
        binding: &SourceLeaseMaintenanceBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceLeaseMaintenancePreparationError> {
        if self.custody.registered.is_some() {
            return Err(LifecycleAllocationError::Unknown.into());
        }
        self.validate_registered_lease_binding(binding, registration)?;
        let custody = registration
            .retain_registered_database_boundary(&self.custody.writer().database)
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        self.validate_lease_binding(binding, registration)?;
        custody
            .validate(&self.custody.writer().database, registration)
            .map_err(|_| LifecycleAllocationError::Unknown)?;
        self.custody.registered = Some(custody);
        Ok(())
    }
}
