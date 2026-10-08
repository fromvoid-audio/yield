//! Complete original-worker checkpoints; no public refresh before full success.
use super::*;

impl OwnerRuntime {
    fn current_legacy_prefix(
        &self,
        retained: &RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<
        super::super::super::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        OperationJournalOwnerError,
    > {
        let args = retained
            .source
            .prepare_legacy_adoption_input_until(
                profile,
                self.store(),
                retained.args.participant_id(),
                retained.args.source_command_id(),
                &AtomicBool::new(false),
                deadline,
            )
            .map_err(|_| LegacyAdoptionCustodyError::ReserveRecovery)?;
        if args.original_image_digest() != retained.args.original_image_digest()
            || args.source_database_identity() != retained.args.source_database_identity()
            || args.source_reservation_identity() != retained.args.source_reservation_identity()
            || args.profile_owner_identity() != retained.args.profile_owner_identity()
        {
            return Err(LegacyAdoptionCustodyError::ReserveRecovery.into());
        }
        Ok(args)
    }
    pub(in crate::operation_journal::owner) fn verify_legacy_prefix(
        &self,
        retained: &RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<(), OperationJournalOwnerError> {
        let current = self.current_legacy_prefix(retained, profile, deadline)?;
        if current
            != retained
                .volumes
                .as_ref()
                .ok_or(LegacyAdoptionCustodyError::Closed)?
                .checkpoint
        {
            return Err(LegacyAdoptionCustodyError::ReserveRecovery.into());
        }
        Ok(())
    }
    pub(super) fn advance_legacy_prefix(
        &self,
        retained: &mut RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<(), OperationJournalOwnerError> {
        let current = self.current_legacy_prefix(retained, profile, deadline)?;
        retained
            .volumes
            .as_mut()
            .ok_or(LegacyAdoptionCustodyError::Closed)?
            .checkpoint = current;
        Ok(())
    }
}
