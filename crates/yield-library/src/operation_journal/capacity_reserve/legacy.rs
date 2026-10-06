//! Original sealed source custody immediately before each reserve-file effect.
use super::*;
use crate::sample_sources::db::LegacyReserveUseProof;

struct LegacyBackend<'a, 'b, B> {
    proof: &'a LegacyReserveUseProof<'b>,
    backend: B,
}
impl<B: ReserveBackend> ReserveBackend for LegacyBackend<'_, '_, B> {
    fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
        self.proof
            .validate_for_bootstrap()
            .map_err(|_| ReserveFailureReason::LegacyCustodyClosed)?;
        self.backend.preallocate(file, bytes)
    }
    fn set_len(&self, file: &File, bytes: u64) -> Result<(), ReserveFailureReason> {
        self.proof
            .validate_for_bootstrap()
            .map_err(|_| ReserveFailureReason::LegacyCustodyClosed)?;
        self.backend.set_len(file, bytes)
    }
    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
        self.proof
            .validate_for_bootstrap()
            .map_err(|_| ReserveFailureReason::LegacyCustodyClosed)?;
        self.backend.sync(file)
    }
    fn final_observation(&self, file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
        self.proof
            .validate_for_bootstrap()
            .map_err(|_| ReserveFailureReason::LegacyCustodyClosed)?;
        self.backend.final_observation(file)
    }
}

#[cfg(target_os = "macos")]
pub(in super::super) fn allocate_legacy_protected_reserve(
    file: File,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<ProtectedReserve, ReserveFailure> {
    reserve_with_backend(
        file,
        LegacyBackend {
            proof,
            backend: MacReserveBackend,
        },
    )
}

#[cfg(test)]
mod tests;
