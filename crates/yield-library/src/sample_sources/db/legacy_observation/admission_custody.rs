//! Original source-only revalidation after the worker's own journal checkpoint changes.
use super::*;
use crate::operation_journal::legacy_adoption_commands::{
    LegacyAdoptionIntentArgs, OriginalSourceCheckpoint,
};

impl LegacySourceReadOwner {
    pub(crate) fn validate_admission_checkpoint_until(
        &self,
        profile: &WritableProfileGuard,
        args: &LegacyAdoptionIntentArgs,
        deadline: Instant,
    ) -> Result<()> {
        args.validate().map_err(refused)?;
        // This sealed proof requires the same permanent no-effect connection, original
        // roots/reservation/profile and any explicit separately mounted MAIN reserve root.
        let proof = self.reserve_use_proof(profile, deadline)?;
        let source = self.capture_pager_until(profile, deadline)?;
        let descriptor = self
            .binding
            .root()
            .try_clone_descriptor()
            .map_err(refused)?;
        let directory =
            crate::durable_capacity::descriptor_capacity_facts(&descriptor).map_err(refused)?;
        let (bytes, control) = self
            .reservation
            .observation_lock_facts(profile, &self.binding)
            .map_err(refused)?;
        if !args.matches_source_checkpoint(
            &source,
            profile,
            OriginalSourceCheckpoint {
                main_identity: self.binding.identity(),
                media_identity: self.media_root.identity(),
                directory_identity: self.binding.root().identity(),
                reservation_identity: self.reservation.observation_lock_identity(),
                directory: &directory,
                control: &control,
                control_bytes: bytes,
            },
        ) {
            return Err(LegacyObservationError(
                "original admitted source checkpoint changed",
            ));
        }
        // Final checks share the original deadline; observation cannot renew it or
        // promote the permanently fenced source into a mutable connection.
        proof.validate_for_bootstrap()?;
        if Instant::now() >= deadline {
            return Err(LegacyObservationError(
                "original admission checkpoint deadline elapsed",
            ));
        }
        Ok(())
    }
}
