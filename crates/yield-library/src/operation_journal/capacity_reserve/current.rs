//! Original sealed source custody immediately before each reserve-file effect.
use super::*;
use crate::sample_sources::db::CurrentReserveUseProof;

struct CurrentBackend<'a, 'b, B> {
    proof: &'a CurrentReserveUseProof<'b>,
    backend: B,
}
impl<B: ReserveBackend> ReserveBackend for CurrentBackend<'_, '_, B> {
    fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
        let _effect = self
            .proof
            .begin_bootstrap_effect()
            .map_err(|_| ReserveFailureReason::CurrentCustodyClosed)?;
        self.backend.preallocate(file, bytes)
    }
    fn set_len(&self, file: &File, bytes: u64) -> Result<(), ReserveFailureReason> {
        let _effect = self
            .proof
            .begin_bootstrap_effect()
            .map_err(|_| ReserveFailureReason::CurrentCustodyClosed)?;
        self.backend.set_len(file, bytes)
    }
    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
        let _effect = self
            .proof
            .begin_bootstrap_effect()
            .map_err(|_| ReserveFailureReason::CurrentCustodyClosed)?;
        self.backend.sync(file)
    }
    fn final_observation(&self, file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
        let _effect = self
            .proof
            .begin_bootstrap_effect()
            .map_err(|_| ReserveFailureReason::CurrentCustodyClosed)?;
        self.backend.final_observation(file)
    }
}

#[cfg(target_os = "macos")]
pub(in super::super) fn allocate_current_protected_reserve(
    file: File,
    proof: &CurrentReserveUseProof<'_>,
) -> Result<ProtectedReserve, ReserveFailure> {
    reserve_with_backend(
        file,
        CurrentBackend {
            proof,
            backend: MacReserveBackend,
        },
    )
}
