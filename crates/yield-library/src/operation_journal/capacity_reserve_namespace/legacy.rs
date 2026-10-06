//! Fixed reserve child effects require genuine original pre-lease source custody.
use super::*;
use crate::sample_sources::db::LegacyReserveUseProof;

pub(in super::super) fn create_owned_legacy_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    observe(ReserveAttempt::Create, create(root, profile, proof))
}

fn create<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    #[cfg(not(target_os = "macos"))]
    {
        // Original physical gate stays first: unsupported targets perform no namespace effect.
        let _ = (root, profile, proof);
        Err(unsupported_failure())
    }
    #[cfg(target_os = "macos")]
    {
        let file = open_legacy_new(root, profile, proof)?;
        let reserve =
            super::super::capacity_reserve::allocate_legacy_protected_reserve(file, proof)
                .map_err(|failure| {
                    let reason = failure.reason().clone();
                    let allocation = failure.observed_allocation();
                    ReserveNamespaceFailure::new(
                        ReserveNamespaceFailureStage::Allocation,
                        ReserveNamespaceFailureReason::Allocation(reason),
                        Some(OwnedFailureFile::Raw(failure.into_file())),
                        None,
                        allocation,
                    )
                })?;
        finish_legacy_created_reserve(root, profile, reserve, proof)
    }
}

#[cfg(any(target_os = "macos", test))]
fn open_legacy_new(
    root: &BoundDatabaseRoot,
    profile: &WritableProfileGuard,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<File, ReserveNamespaceFailure> {
    validate_root(root, profile).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::RootValidation,
            reason,
            None,
            None,
            None,
        )
    })?;
    validate_root_identity(root.identity()).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Observation,
            reason,
            None,
            None,
            None,
        )
    })?;
    proof.validate_root_for_bootstrap(root).map_err(|_| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Open,
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::LegacyCustodyClosed),
            None,
            None,
            None,
        )
    })?;
    open_new(root).map_err(|reason| {
        ReserveNamespaceFailure::new(ReserveNamespaceFailureStage::Open, reason, None, None, None)
    })
}

#[cfg(any(target_os = "macos", test))]
fn finish_legacy_created_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    let observation = match ReserveNamespaceObservation::from_reserve(root.identity(), &reserve) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Observation,
                ReserveNamespaceFailureReason::InvalidObservation(reason),
                Some(OwnedFailureFile::Protected(reserve)),
                None,
                None,
            ));
        }
    };
    if proof.validate_root_for_bootstrap(root).is_err() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Sync,
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::LegacyCustodyClosed),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if let Err(error) = root.sync_namespace(profile) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Sync,
            ReserveNamespaceFailureReason::Sync(error.to_string()),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if let Err(reason) = validate_root(root, profile) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if proof.validate_root_for_bootstrap(root).is_err() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::LegacyCustodyClosed),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    #[cfg(target_os = "macos")]
    {
        bind_after_sync(root, profile, reserve, observation)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Unsupported,
            ReserveNamespaceFailureReason::Unsupported,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ))
    }
}

#[cfg(test)]
mod tests;
