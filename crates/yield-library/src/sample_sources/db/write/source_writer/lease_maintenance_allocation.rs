//! Actual pre-audit-capable source lease-maintenance observations, never a release grant.
use super::*;
use crate::lifecycle_allocation::{
    self, LifecycleAllocationError, LifecycleAllocationRole, LifecycleDatabaseAllocation,
};
use std::{
    sync::Weak,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Passive exact source/lease/root observations and finite storage plan from the original writer.
/// A completed audit is optional; this value cannot become an authoritative source revision.
#[derive(Clone, Debug)]
pub struct SourceLeaseMaintenanceAllocation {
    subject: super::super::super::SourceRegistryReferenceSubject,
    allocation: LifecycleDatabaseAllocation,
    owner: Weak<()>,
    deadline: Instant,
}
impl SourceLeaseMaintenanceAllocation {
    /// Historical original source/lifecycle/lease tuple; it conveys no live source custody.
    pub fn subject(&self) -> &super::super::super::SourceRegistryReferenceSubject {
        &self.subject
    }
    /// Actual retained SQLite/VFS observations, never physical qualification or permission.
    pub fn allocation(&self) -> &LifecycleDatabaseAllocation {
        &self.allocation
    }
    /// Original common planning deadline; revalidation cannot extend it.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
impl SourceWriter {
    /// Observe this original source MAIN's bounded lease-maintenance policy before admission.
    /// Performs no lease renewal, audit fabrication, SQL mutation, capacity claim or reservation.
    pub fn capture_lease_maintenance_allocation(
        &self,
        deadline: Instant,
    ) -> Result<SourceLeaseMaintenanceAllocation, LifecycleAllocationError> {
        use LifecycleAllocationError::Unknown;
        if !self.database.connection.is_autocommit() {
            return Err(Unknown);
        }
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope().ok_or(Unknown)?;
        lifecycle_allocation::bounded_read(&self.database.connection, deadline, || {
            let lease = self.database.source_lease.as_deref().ok_or(Unknown)?;
            let expiry = lease.validate_current_owner().map_err(|_| Unknown)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(LifecycleAllocationError::Budget)?;
            let required = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Unknown)?
                .checked_add(remaining)
                .and_then(|v| v.as_secs().checked_add(u64::from(v.subsec_nanos() != 0)))
                .and_then(|v| i64::try_from(v).ok())
                .ok_or(Unknown)?;
            if expiry <= required {
                return Err(Unknown);
            }
            super::super::super::schema::require_known_metadata_schema(&self.database.connection)
                .map_err(|_| Unknown)?;
            let before = self
                .capture_registry_reference_observation(None)
                .map_err(|_| Unknown)?;
            let binding = self
                .database
                .database_file_binding
                .as_deref()
                .ok_or(Unknown)?;
            let allocation = lifecycle_allocation::capture(
                &self.database.connection,
                binding.root(),
                binding.name(),
                LifecycleAllocationRole::SourceLeaseMaintenance,
                deadline,
            )?;
            let after = self
                .capture_registry_reference_observation(None)
                .map_err(|_| Unknown)?;
            super::super::super::schema::require_known_metadata_schema(&self.database.connection)
                .map_err(|_| Unknown)?;
            if before.subject() != after.subject()
                || lifecycle_allocation::capture(
                    &self.database.connection,
                    binding.root(),
                    binding.name(),
                    LifecycleAllocationRole::SourceLeaseMaintenance,
                    deadline,
                )? != allocation
            {
                return Err(Unknown);
            }
            Ok(SourceLeaseMaintenanceAllocation {
                subject: before.subject().clone(),
                allocation,
                owner: Arc::downgrade(&self.reference_liveness),
                deadline,
            })
        })
    }

    /// Reobserve with the original live writer/deadline. Matching stored tuples never rebind
    /// another acquisition and no result grants preparation, admitted release or close evidence.
    pub fn validate_lease_maintenance_allocation(
        &self,
        observation: &SourceLeaseMaintenanceAllocation,
    ) -> Result<(), LifecycleAllocationError> {
        if !Weak::ptr_eq(
            &observation.owner,
            &Arc::downgrade(&self.reference_liveness),
        ) {
            return Err(LifecycleAllocationError::Unknown);
        }
        let actual = self.capture_lease_maintenance_allocation(observation.deadline)?;
        if actual.subject != observation.subject || actual.allocation != observation.allocation {
            return Err(LifecycleAllocationError::Unknown);
        }
        Ok(())
    }
}
