//! Original Registered lifetime custody. No write, decision, capacity or settlement permission.
use super::*;

pub(crate) struct RegisteredDatabaseCustody {
    // Count must release before the final lease reference can trigger lease cleanup.
    _use: SourceEffectPermit,
    lease: Arc<SourceLease>,
    request: Arc<SourceAuthorityRequest>,
}
const _: () = assert!(std::mem::size_of::<RegisteredDatabaseCustody>() <= 512);

impl SourceAuthorityRegistration {
    pub(crate) fn retain_registered_database_boundary(
        &self,
        database: &SourceDatabase,
    ) -> Result<RegisteredDatabaseCustody, SourceAuthorityError> {
        self.validate_registered_database_boundary(database)?;
        let pin = self.guard.pin_for_effect()?;
        let counted = pin.begin_effect()?;
        // The count stays held after this second CONTROL/profile/current-registration check.
        self.validate_registered_database_boundary(database)?;
        Ok(RegisteredDatabaseCustody {
            _use: counted,
            lease: Arc::clone(&pin.lease),
            request: Arc::clone(&self.request),
        })
    }
    pub(crate) fn close_database_with_registered_custody(
        &mut self,
        database: SourceDatabase,
        custody: Option<RegisteredDatabaseCustody>,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        database.close_with_registered_custody(&mut self.guard, custody)
    }
}
impl RegisteredDatabaseCustody {
    /// The unchanged original request observation, never current authority by itself.
    pub(crate) fn registration_generation(&self) -> u64 {
        self.request.registration_generation()
    }

    pub(crate) fn validate(
        &self,
        database: &SourceDatabase,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceAuthorityError> {
        if !Arc::ptr_eq(&self.request, &registration.request)
            || !database
                .source_lease
                .as_ref()
                .is_some_and(|lease| Arc::ptr_eq(lease, &self.lease))
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        registration.validate_registered_database_boundary(database)
    }
    pub(super) fn validate_close(
        &self,
        database: &SourceDatabase,
        guard: &SourceAuthorityGuard,
    ) -> Result<(), SourceAuthorityError> {
        if !Arc::ptr_eq(&self.request.state, &guard.state)
            || !database
                .source_lease
                .as_ref()
                .is_some_and(|lease| Arc::ptr_eq(lease, &self.lease))
            || !guard.belongs_to(&self.lease)
            || guard.state.phase() != DRAINING
        {
            return Err(SourceAuthorityError::GuardMismatch);
        }
        Ok(())
    }
    pub(crate) fn close_ready(&self, database: &SourceDatabase) -> bool {
        database
            .source_lease
            .as_ref()
            .is_some_and(|lease| Arc::ptr_eq(lease, &self.lease) && Arc::strong_count(lease) <= 3)
    }
}
