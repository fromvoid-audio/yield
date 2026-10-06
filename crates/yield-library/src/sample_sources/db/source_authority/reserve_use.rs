//! Original Current registration scope; no caller-selected descriptor or refreshed deadline.
use super::*;
use crate::app_dirs::WritableProfileGuard;
use std::fs::File;

pub(crate) struct CurrentReserveUseProof<'a> {
    main: File,
    descriptor: File,
    root: &'a BoundDatabaseRoot,
    request: &'a SourceAuthorityRequest,
    proof: &'a SourceAuthorityUseProof,
    profile: &'a WritableProfileGuard,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::AppRootGuard;

    #[test]
    fn current_reserve_scope_binds_original_request_root_proof_and_deadline() {
        let app = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(std::fs::canonicalize(app.path()).unwrap()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let media_a = tempfile::tempdir().unwrap();
        let db_a = tempfile::tempdir().unwrap();
        let media_b = tempfile::tempdir().unwrap();
        let db_b = tempfile::tempdir().unwrap();
        let database_a = SourceDatabase::open_for_source_write_with_profile_guard(
            media_a.path(),
            &profile,
            db_a.path(),
        )
        .unwrap();
        let database_b = SourceDatabase::open_for_source_write_with_profile_guard(
            media_b.path(),
            &profile,
            db_b.path(),
        )
        .unwrap();
        let (a, guard_a) = database_a.prepare_source_authority().unwrap().split();
        let (b, guard_b) = database_b.prepare_source_authority().unwrap().split();
        let proof_a = a.accept_confirmation(guard_a.confirm().unwrap()).unwrap();
        let proof_b = b.accept_confirmation(guard_b.confirm().unwrap()).unwrap();
        assert!(matches!(
            a.reserve_use(b.media_root(), &proof_a, &profile),
            Err(SourceAuthorityError::RegistrationMismatch)
        ));
        assert!(matches!(
            a.reserve_use(a.media_root(), &proof_b, &profile),
            Err(SourceAuthorityError::RegistrationMismatch)
        ));
        for root in [a.media_root(), a.database_root()] {
            let scope = a.reserve_use(root, &proof_a, &profile).unwrap();
            assert_eq!(
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                    scope.descriptor()
                )
                .as_deref(),
                Some(root.identity())
            );
            scope.validate_for_bootstrap().unwrap();
        }
        let expired = proof_a.with_expired_deadline_for_tests();
        assert!(matches!(
            a.reserve_use(a.media_root(), &expired, &profile),
            Err(SourceAuthorityError::ConfirmationStale)
        ));
        let scope = b
            .reserve_use(b.database_root(), &proof_b, &profile)
            .unwrap();
        drop(guard_b);
        assert!(scope.validate_for_bootstrap().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn current_reserve_scope_refuses_replaced_original_root_without_substitute_effects() {
        let app = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(std::fs::canonicalize(app.path()).unwrap()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let media = parent.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let db = tempfile::tempdir().unwrap();
        let database =
            SourceDatabase::open_for_source_write_with_profile_guard(&media, &profile, db.path())
                .unwrap();
        let (request, guard) = database.prepare_source_authority().unwrap().split();
        let proof = request
            .accept_confirmation(guard.confirm().unwrap())
            .unwrap();
        let scope = request
            .reserve_use(request.media_root(), &proof, &profile)
            .unwrap();
        let original = crate::filesystem_identity::stable_filesystem_identity_from_open_file(
            scope.descriptor(),
        );
        std::fs::rename(&media, parent.path().join("held-original")).unwrap();
        std::fs::create_dir(&media).unwrap();
        assert!(scope.validate_for_bootstrap().is_err());
        assert_eq!(
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                scope.descriptor()
            ),
            original
        );
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);
    }
}

impl SourceAuthorityRequest {
    pub(crate) fn reserve_use<'a>(
        &'a self,
        root: &'a BoundDatabaseRoot,
        proof: &'a SourceAuthorityUseProof,
        profile: &'a WritableProfileGuard,
    ) -> Result<CurrentReserveUseProof<'a>, SourceAuthorityError> {
        if !(std::ptr::eq(root, &self.media_root) || std::ptr::eq(root, &self.database_root))
            || self.registration_id != proof.registration_id
            || self.lease_identity != proof.lease_identity
            || !Arc::ptr_eq(&self.state, &proof.state)
            || !Arc::ptr_eq(&self.state.lease_liveness, &proof.lease_liveness)
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        self.validate_consuming_boundary(profile)?;
        proof.validate_for_physical_use()?;
        let descriptor = root.try_clone_descriptor().map_err(|source| {
            SourceAuthorityError::Lease(SourceLeaseError::Io {
                path: root.path().to_path_buf(),
                source,
            })
        })?;
        let binding = &self.database_file_binding;
        let main = binding
            .root()
            .open_file_read_nofollow(binding.name())
            .map_err(|source| {
                SourceAuthorityError::Lease(SourceLeaseError::Io {
                    path: binding.path().to_path_buf(),
                    source,
                })
            })?;
        let database_descriptor = self
            .database_root
            .try_clone_descriptor()
            .map_err(|source| {
                SourceAuthorityError::Lease(SourceLeaseError::Io {
                    path: self.database_root.path().to_path_buf(),
                    source,
                })
            })?;
        let facts = crate::durable_capacity::descriptor_capacity_facts;
        // A separately mounted MAIN needs a genuinely retained same-volume reserve root.
        // Current registration exposes only media/database roots, so refuse absent that scope.
        if facts(&main)
            .map_err(|_| SourceAuthorityError::RegistrationMismatch)?
            .identity
            != facts(&database_descriptor)
                .map_err(|_| SourceAuthorityError::RegistrationMismatch)?
                .identity
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        let scope = CurrentReserveUseProof {
            main,
            descriptor,
            root,
            request: self,
            proof,
            profile,
        };
        scope.validate_for_bootstrap()?;
        Ok(scope)
    }
}

impl CurrentReserveUseProof<'_> {
    pub(crate) fn descriptor(&self) -> &File {
        &self.descriptor
    }

    pub(crate) fn validate_for_bootstrap(&self) -> Result<(), SourceAuthorityError> {
        self.request.validate_consuming_boundary(self.profile)?;
        self.proof.validate_for_physical_use()?;
        if crate::filesystem_identity::stable_filesystem_identity_from_open_file(&self.descriptor)
            .as_deref()
            != Some(self.root.identity())
            || crate::filesystem_identity::stable_filesystem_identity_from_open_file(&self.main)
                .as_deref()
                != Some(self.request.database_file_identity())
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        // Recheck the unchanged original deadline after native observations.
        self.proof.validate_for_physical_use()
    }
}
