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
    retirement: Option<super::lease_maintenance_preparation::retirement_policy::Input>,
}
impl SourceLeaseMaintenanceAllocation {
    /// Attach one complete frozen family to this genuine original observation. This only
    /// requests original-owner planning; it cannot issue a positive policy or effect grant.
    pub(crate) fn bind_retirement_policy(
        &mut self,
        args: &crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs,
    ) -> Result<(), LifecycleAllocationError> {
        if self.retirement.is_some()
            || !args
                .lease_fence()
                .matches_allocation(&self.subject, &self.allocation)
        {
            return Err(LifecycleAllocationError::Unknown);
        }
        self.retirement =
            Some(super::lease_maintenance_preparation::retirement_policy::Input::new(args)?);
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn replace_retirement_policy_for_test(
        &mut self,
        args: Option<&crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs>,
    ) -> Result<(), LifecycleAllocationError> {
        self.retirement = None;
        if let Some(args) = args {
            self.bind_retirement_policy(args)?;
        }
        Ok(())
    }

    pub(super) fn retirement_input(
        &self,
    ) -> Option<super::lease_maintenance_preparation::retirement_policy::Input> {
        self.retirement
    }

    pub(crate) fn preparation_scratch_bytes(&self) -> Option<usize> {
        // Actual recaptured allocation/subjects and checked CONTROL/native scratch; no
        // StoredSourceLeaseFence encoding is created during the preparation phase.
        self.retained_input_bytes()?
            .checked_mul(3)?
            .checked_add(12 * 1024)
    }
    pub(crate) fn binding_scratch_bytes(&self) -> Option<usize> {
        // A returned subject/descriptor and Vec growth during sequential validation and
        // canonical serialization. The CONTROL/native scratch is independently included.
        crate::operation_journal::source_lease_binding::StoredSourceLeaseFence::capture_encoding_bound(&self.subject, &self.allocation)?
            .checked_mul(3)?.checked_add(12 * 1024)
    }

    pub(crate) fn retained_input_bytes(&self) -> Option<usize> {
        std::mem::size_of::<Self>()
            .checked_add(self.subject.retained_heap_bytes()?)?
            .checked_add(self.allocation.retained_heap_bytes()?)
    }
    // Original live custody and native facts only. Full subject/schema/pager facts were
    // validated at entry; the exclusive empty writer prevents intervening row mutations.
    pub(super) fn validate_parked(
        &self,
        writer: &SourceWriter,
        deadline: Instant,
    ) -> Result<(), LifecycleAllocationError> {
        use LifecycleAllocationError::{Budget, Unknown};
        if deadline > self.deadline || Instant::now() >= deadline {
            return Err(Budget);
        }
        if !Weak::ptr_eq(&self.owner, &Arc::downgrade(&writer.reference_liveness))
            || self.subject.source_id() != writer.source_id.as_str()
            || self.subject.lifecycle_generation() != writer.lifecycle_generation
        {
            return Err(Unknown);
        }
        let lease = writer.database.source_lease.as_deref().ok_or(Unknown)?;
        require_expiry(
            lease.validate_current_owner().map_err(|_| Unknown)?,
            deadline,
        )?;
        let binding = writer
            .database
            .database_file_binding
            .as_deref()
            .ok_or(Unknown)?;
        self.allocation.validate_parked_objects(
            &writer.database.connection,
            binding.root(),
            binding.name(),
            deadline,
        )?;
        require_expiry(
            lease.validate_current_owner().map_err(|_| Unknown)?,
            deadline,
        )?;
        if Instant::now() >= deadline {
            return Err(Budget);
        }
        Ok(())
    }

    pub(super) fn validate_retained(
        &self,
        writer: &SourceWriter,
        deadline: Instant,
        entered: bool,
    ) -> Result<(), LifecycleAllocationError> {
        use LifecycleAllocationError::Unknown;
        if !Weak::ptr_eq(&self.owner, &Arc::downgrade(&writer.reference_liveness)) {
            return Err(Unknown);
        }
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope().ok_or(Unknown)?;
        lifecycle_allocation::bounded_read(&writer.database.connection, deadline, || {
            let lease = writer.database.source_lease.as_deref().ok_or(Unknown)?;
            let expiry = lease.validate_current_owner().map_err(|_| Unknown)?;
            require_expiry(expiry, deadline)?;
            super::super::super::schema::require_known_metadata_schema(&writer.database.connection)
                .map_err(|_| Unknown)?;
            let scan = writer.current_scan_fence().map_err(|_| Unknown)?;
            let actual = if writer.database.connection.is_autocommit() {
                writer.capture_registry_reference_observation(None)
            } else {
                super::super::super::source_registry_reference::capture_in_transaction(
                    &writer.database,
                    &scan,
                )
            }
            .map_err(|_| Unknown)?;
            if actual.subject() != &self.subject {
                return Err(Unknown);
            }
            let binding = writer
                .database
                .database_file_binding
                .as_deref()
                .ok_or(Unknown)?;
            let allocation = lifecycle_allocation::capture(
                &writer.database.connection,
                binding.root(),
                binding.name(),
                LifecycleAllocationRole::SourceLeaseMaintenance,
                deadline,
            )?;
            if (entered && !self.allocation.matches_entered(&allocation))
                || (!entered && self.allocation != allocation)
            {
                return Err(Unknown);
            }
            if lifecycle_allocation::capture(
                &writer.database.connection,
                binding.root(),
                binding.name(),
                LifecycleAllocationRole::SourceLeaseMaintenance,
                deadline,
            )? != allocation
            {
                return Err(Unknown);
            }
            super::super::super::schema::require_known_metadata_schema(&writer.database.connection)
                .map_err(|_| Unknown)?;
            writer.validate_scan_fence(&scan).map_err(|_| Unknown)?;
            lease.validate_current_owner().map_err(|_| Unknown)?;
            Ok(())
        })
    }

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
            require_expiry(expiry, deadline)?;
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
                retirement: None,
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

fn require_expiry(expiry: i64, deadline: Instant) -> Result<(), LifecycleAllocationError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(LifecycleAllocationError::Budget)?;
    let required = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LifecycleAllocationError::Unknown)?
        .checked_add(remaining)
        .and_then(|v| v.as_secs().checked_add(u64::from(v.subsec_nanos() != 0)))
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(LifecycleAllocationError::Unknown)?;
    if expiry <= required {
        Err(LifecycleAllocationError::Unknown)
    } else {
        Ok(())
    }
}
