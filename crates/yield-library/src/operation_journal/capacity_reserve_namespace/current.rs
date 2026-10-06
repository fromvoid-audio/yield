//! Fixed reserve child effects require genuine original Current source custody.
use super::*;
use crate::sample_sources::db::CurrentReserveUseProof;

pub(in super::super) fn create_owned_current_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    proof: &CurrentReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    observe(ReserveAttempt::Create, create(root, profile, proof))
}

fn create<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    proof: &CurrentReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    #[cfg(not(target_os = "macos"))]
    {
        // Original physical gate stays first: unsupported targets perform no namespace effect.
        let _ = (root, profile, proof);
        Err(unsupported_failure())
    }
    #[cfg(target_os = "macos")]
    {
        let file = open_current_new(root, profile, proof)?;
        let reserve =
            super::super::capacity_reserve::allocate_current_protected_reserve(file, proof)
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
        finish_current_created_reserve(root, profile, reserve, proof)
    }
}

#[cfg(any(target_os = "macos", test))]
fn open_current_new(
    root: &BoundDatabaseRoot,
    profile: &WritableProfileGuard,
    proof: &CurrentReserveUseProof<'_>,
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
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::CurrentCustodyClosed),
            None,
            None,
            None,
        )
    })?;
    let _effect = proof.begin_bootstrap_effect().map_err(|_| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Open,
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::CurrentCustodyClosed),
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
fn finish_current_created_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    proof: &CurrentReserveUseProof<'_>,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    finish_current_created_reserve_inner(
        root,
        profile,
        reserve,
        proof,
        #[cfg(test)]
        None,
    )
}

#[cfg(any(target_os = "macos", test))]
fn finish_current_created_reserve_inner<'a>(
    root: &'a BoundDatabaseRoot,
    profile: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    proof: &CurrentReserveUseProof<'_>,
    #[cfg(test)] after_sync: Option<fn(u8)>,
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
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::CurrentCustodyClosed),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if let Err(error) = root.sync_current_namespace(
        profile,
        proof,
        #[cfg(test)]
        after_sync,
    ) {
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
            ReserveNamespaceFailureReason::Allocation(ReserveFailureReason::CurrentCustodyClosed),
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
mod tests {
    use super::*;
    use crate::{app_dirs::AppRootGuard, sample_sources::SourceDatabase};
    use std::io::Write;

    #[test]
    fn current_original_namespace_refusal_retains_open_file_and_unchanged_extent() {
        for before_create in [true, false] {
            let app = tempfile::tempdir().unwrap();
            let _app = AppRootGuard::set(app.path().to_owned()).unwrap();
            let profile = WritableProfileGuard::acquire_current().unwrap();
            let media = tempfile::tempdir().unwrap();
            let db = tempfile::tempdir().unwrap();
            let source = SourceDatabase::open_for_source_write_with_profile_guard(
                media.path(),
                &profile,
                db.path(),
            )
            .unwrap();
            let (request, guard) = source.prepare_source_authority().unwrap().split();
            let proof = request
                .accept_confirmation(guard.confirm().unwrap())
                .unwrap();
            let root = request.media_root();
            let scope = request.reserve_use(root, &proof, &profile).unwrap();
            let path = root.path().join(RESERVE_FILE_NAME);
            if before_create {
                drop(guard);
                let failure = open_current_new(root, &profile, &scope).unwrap_err();
                assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Open);
                assert!(failure.owned_file.is_none());
                assert!(!path.exists());
            } else {
                let mut file = open_current_new(root, &profile, &scope).unwrap();
                file.write_all(&[0x6b; 4096]).unwrap();
                let identity =
                    crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file)
                        .unwrap();
                // Logical negative-only reserve fixture cannot pass native physical admission.
                let reserve = super::super::super::capacity_reserve::test_fixture_reserve(file);
                drop(guard);
                let failure = finish_current_created_reserve(root, &profile, reserve, &scope)
                    .err()
                    .unwrap();
                assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Sync);
                let backing = failure.into_legacy_backing();
                let held = backing.file.as_ref().unwrap();
                assert_eq!(
                    crate::filesystem_identity::stable_filesystem_identity_from_open_file(held)
                        .unwrap(),
                    identity
                );
                assert_eq!(held.metadata().unwrap().len(), 4096);
                assert_eq!(std::fs::read(&path).unwrap(), vec![0x6b; 4096]);
                drop(backing);
                assert_eq!(std::fs::read(&path).unwrap(), vec![0x6b; 4096]);
            }
        }
    }
}
