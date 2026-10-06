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
    registration_generation: u64,
    retirement: Option<super::retirement_policy::SourceRetirementForwardPolicy>,
    issuer: Weak<()>,
    nonce: Uuid,
    deadline: Instant,
}
impl SourceLeaseMaintenanceBinding {
    /// Captured from the genuine original request; not decoded from a historical fence.
    pub(crate) fn registration_generation(&self) -> u64 {
        self.registration_generation
    }

    #[cfg(test)]
    pub(super) fn set_registration_generation_for_tests(&mut self, generation: u64) {
        self.registration_generation = generation;
    }

    pub(crate) fn retirement_forward_policy(
        &self,
    ) -> Option<&super::retirement_policy::SourceRetirementForwardPolicy> {
        self.retirement.as_ref()
    }
    pub(crate) fn retained_memory_bytes(&self) -> Option<usize> {
        std::mem::size_of::<Self>().checked_add(self.stored.retained_heap_bytes()?)
    }
    /// Borrow historical finite scope. Prepared/registered phase is not full journal readiness.
    pub fn stored_fence(&self) -> &StoredSourceLeaseFence {
        &self.stored
    }
}
impl SourceLeaseMaintenancePreparation {
    /// Seal planning only after this same original preparation retains Registered custody.
    /// Generic pre-audit diagnostics retain no retirement policy; no fallback can issue one.
    pub(crate) fn capture_retirement_forward_policy(
        &self,
        binding: &mut SourceLeaseMaintenanceBinding,
        registration: &SourceAuthorityRegistration,
        encoded: &[u8],
    ) -> Result<(), SourceLeaseMaintenancePreparationError> {
        let Some(input) = self.observation().retirement_input() else {
            return Ok(());
        };
        if binding.retirement.is_some()
            || self.custody.registered.is_none()
            || self.custody.policy.is_none()
        {
            return Err(LifecycleAllocationError::Unknown.into());
        }
        self.validate_registered_lease_binding(binding, registration)?;
        let policy = super::retirement_policy::SourceRetirementForwardPolicy::capture(
            input,
            &binding.stored,
            self.nonce(),
            encoded,
        )?;
        self.validate_registered_lease_binding(binding, registration)?;
        binding.retirement = Some(policy);
        Ok(())
    }

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
            registration_generation: registration.registration_generation(),
            retirement: None,
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
        if binding.registration_generation != registration.registration_generation()
            || binding.nonce != self.nonce()
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
            if custody.registration_generation() != binding.registration_generation {
                return Err(LifecycleAllocationError::Unknown.into());
            }
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
