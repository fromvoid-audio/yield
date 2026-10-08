//! First lease row on the original admitted source; no lease/audit grant is reconstructed.
use super::super::schema;
use super::migration::VerifiedLegacyMigration;
use super::*;
use crate::operation_journal::{OperationId, owner::OriginalLegacyBootstrapAdmission};

pub(crate) struct VerifiedLegacyBootstrap {
    pub(crate) operation: OperationId,
    owner_identity: String,
    epoch: i64,
    expected: schema::SchemaImage,
}
pub(crate) enum LegacyBootstrapSettlement {
    NotStarted,
    RollbackConfirmed,
    Unknown {
        candidate: Option<VerifiedLegacyBootstrap>,
        restored: bool,
    },
    Committed(VerifiedLegacyBootstrap),
}
impl LegacySourceReadOwner {
    pub(crate) fn bootstrap_original_admitted(
        &self,
        admission: &OriginalLegacyBootstrapAdmission<'_>,
    ) -> LegacyBootstrapSettlement {
        use LegacyBootstrapSettlement as S;
        let (profile, request, committed, deadline) = admission.bootstrap_inputs();
        if admission
            .validate_original_connection(&self.connection)
            .is_err()
        {
            return S::NotStarted;
        }
        // Exactly one compiled table is added. Its INTEGER PRIMARY KEY creates no index.
        let mut expected = committed.expected.clone();
        expected.push((
            "table".into(),
            "source_lease".into(),
            "source_lease".into(),
            Some(super::super::source_lease::LEASE_TABLE_SQL.into()),
        ));
        expected.sort();
        let owner_identity = uuid::Uuid::new_v4().to_string();
        let Ok(scope) = self.connection.enter_original_legacy_bootstrap(admission) else {
            return S::NotStarted;
        };
        if self.connection.execute_batch("BEGIN EXCLUSIVE").is_err() {
            return if self.connection.is_autocommit() && scope.finish().is_ok() {
                S::NotStarted
            } else {
                S::Unknown {
                    candidate: None,
                    restored: false,
                }
            };
        }
        let progress = Progress::install(&self.connection, deadline);
        let body = (|| -> Result<i64> {
            self.verify_committed_migration_inside_bootstrap(
                profile, request, committed, deadline,
            )?;
            let epoch = self
                .reservation
                .install_original_lease_row(
                    &self.connection,
                    profile,
                    &self.binding,
                    &owner_identity,
                )
                .map_err(refused)?;
            schema::validate_bounded_schema(&self.connection, &expected).map_err(refused)?;
            self.reservation
                .verify_original_lease_row(
                    &self.connection,
                    profile,
                    &self.binding,
                    &owner_identity,
                    epoch,
                )
                .map_err(refused)?;
            if rows::capture_preserved(&self.connection, deadline)?.digest != committed.rows_digest
                || Instant::now() >= deadline
            {
                return Err(LegacyObservationError(
                    "lease bootstrap preserved image changed",
                ));
            }
            self.validate_migration_bindings(profile)?;
            Ok(epoch)
        })();
        drop(progress);
        let epoch = match body {
            Ok(epoch) => epoch,
            Err(_) => {
                let rollback = self.connection.execute_batch("ROLLBACK").is_ok()
                    && self.connection.is_autocommit();
                let restored = scope.finish().is_ok();
                return if rollback
                    && restored
                    && self
                        .revalidate_committed_migration(profile, request, committed, deadline)
                        .is_ok()
                {
                    S::RollbackConfirmed
                } else {
                    S::Unknown {
                        candidate: None,
                        restored: false,
                    }
                };
            }
        };
        let candidate = VerifiedLegacyBootstrap {
            operation: committed.operation,
            owner_identity,
            epoch,
            expected,
        };
        if self.connection.execute_batch("COMMIT").is_err() {
            let restored = scope.finish().is_ok();
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        #[cfg(all(test, target_os = "macos"))]
        if admission.lose_readback_once() {
            let restored = scope.finish().is_ok();
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        let verified = self
            .verify_original_bootstrap(profile, request, committed, &candidate, deadline)
            .is_ok();
        let restored = scope.finish().is_ok();
        if !restored || !verified {
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        S::Committed(candidate)
    }

    // A transaction retains the exact original binding; only quiescence differs from ordinary
    // committed readback. Check the complete immutable receipt and preserved rows again.
    fn verify_committed_migration_inside_bootstrap(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        committed: &VerifiedLegacyMigration,
        deadline: Instant,
    ) -> Result<()> {
        if self.connection.is_autocommit() {
            return Err(LegacyObservationError("bootstrap transaction absent"));
        }
        self.verify_migration_image(
            profile,
            request,
            committed.operation,
            &committed.encoded,
            &committed.expected,
            &committed.expected,
            committed.rows_digest,
            deadline,
        )
    }

    fn verify_original_bootstrap(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        migration: &VerifiedLegacyMigration,
        bootstrap: &VerifiedLegacyBootstrap,
        deadline: Instant,
    ) -> Result<()> {
        if !self.connection.is_autocommit() || bootstrap.operation != migration.operation {
            return Err(LegacyObservationError(
                "bootstrap commit is not quiescent/current",
            ));
        }
        self.verify_committed_migration(
            profile,
            request,
            migration.operation,
            &migration.encoded,
            &migration.expected,
            &bootstrap.expected,
            migration.rows_digest,
            deadline,
        )?;
        self.reservation
            .verify_original_lease_row(
                &self.connection,
                profile,
                &self.binding,
                &bootstrap.owner_identity,
                bootstrap.epoch,
            )
            .map_err(refused)
    }

    pub(crate) fn revalidate_committed_bootstrap(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        migration: &VerifiedLegacyMigration,
        bootstrap: &VerifiedLegacyBootstrap,
        deadline: Instant,
    ) -> Result<()> {
        self.validate_original_post_migration_policy(request)?;
        self.verify_original_bootstrap(profile, request, migration, bootstrap, deadline)
    }
}
