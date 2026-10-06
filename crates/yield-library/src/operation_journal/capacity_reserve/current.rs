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
        let observation = self.backend.final_observation(file)?;
        self.proof
            .validate_root_for_bootstrap(self.proof.root())
            .map_err(|_| ReserveFailureReason::CurrentCustodyClosed)?;
        Ok(observation)
    }
}

#[cfg(target_os = "macos")]
pub(in super::super) fn allocate_current_protected_reserve(
    file: File,
    proof: &CurrentReserveUseProof<'_>,
) -> Result<ProtectedReserve, ReserveFailure> {
    let _custody = match proof.begin_bootstrap_effect() {
        Ok(custody) => custody,
        Err(_) => {
            return Err(ReserveFailure::new(
                file,
                ReserveFailureStage::InitialValidation,
                ReserveFailureReason::CurrentCustodyClosed,
                None,
            ));
        }
    };
    let reserve = reserve_with_backend(
        file,
        CurrentBackend {
            proof,
            backend: MacReserveBackend,
        },
    )?;
    if proof.validate_root_for_bootstrap(proof.root()).is_err() {
        let allocation = reserve.observation().allocated_bytes;
        return Err(ReserveFailure::new(
            reserve.into_file(),
            ReserveFailureStage::Verification,
            ReserveFailureReason::CurrentCustodyClosed,
            Some(allocation),
        ));
    }
    Ok(reserve)
}
