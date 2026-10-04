//! Owned pre-intent tag policy and empty reservation; no mutation/claim authority.
use super::{
    SourceRevision, SourceTagAdmissionObservation, SourceWalPreparationError, SourceWriter,
    metadata_policy_state::EnteredMetadataPolicy,
    metadata_wal_custody::{Custody, Owner},
    wal_reservation,
};
use std::{fmt, time::Instant};

/// Same-connection tag policy custody and complete frozen scope, never admission.
/// This value stays on its owning thread because it retains the extension-policy read guard.
pub struct SourceTagWalPreparation {
    custody: Custody,
    observation: SourceTagAdmissionObservation,
    deadline: Instant,
}

/// A rejected preparation keeps its genuine owner until cleanup is confirmed.
pub struct SourceTagWalPreparationFailure {
    custody: Custody,
    error: SourceWalPreparationError,
}

impl fmt::Debug for SourceTagWalPreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceTagWalPreparation")
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SourceTagWalPreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceTagWalPreparationFailure")
            .field("error", &self.error())
            .finish()
    }
}

impl SourceWriter {
    /// Retain the original tag observation, settings and empty immediate transaction.
    /// No intent, capacity claim, source/control mutation or tag execution is admitted.
    pub fn prepare_tag_wal_reservation(
        self,
        observation: SourceTagAdmissionObservation,
        requested_deadline: Instant,
    ) -> Result<SourceTagWalPreparation, SourceTagWalPreparationFailure> {
        let deadline = requested_deadline.min(observation.deadline());
        let mut custody = Custody {
            owner: Some(Owner::Unprepared {
                writer: self,
                created_transaction: false,
            }),
            policy: None,
        };
        let result = (|| {
            if !custody.writer().database.connection.is_autocommit() {
                return Err(SourceWalPreparationError::TransactionActive);
            }
            let revision = wal_reservation::validate(
                custody.writer(),
                observation.selection().revision(),
                deadline,
            );
            custody.track_created_transaction();
            let revision = revision?;
            custody
                .writer()
                .validate_tag_scope_in_connection(&observation, &revision, deadline)?;
            custody.policy = Some(EnteredMetadataPolicy::new(
                &custody.writer().database,
                observation.budget().maximum_pages(),
            )?);
            custody
                .policy
                .as_ref()
                .expect("entered tag policy")
                .install(&custody.writer().database)?;
            custody
                .policy
                .as_ref()
                .expect("entered tag policy")
                .validate(&custody.writer().database)?;
            let writer = match custody.owner.take().expect("retained tag owner") {
                Owner::Unprepared { writer, .. } => writer,
                _ => unreachable!(),
            };
            match writer.prepare_policy_wal_reservation(&revision, deadline) {
                Ok(held) => custody.owner = Some(Owner::Held(held)),
                Err(failed) => {
                    custody.owner = Some(Owner::Failed(failed));
                    return Err(SourceWalPreparationError::RollbackUnconfirmed);
                }
            }
            let held = match custody.owner.as_ref().expect("retained tag owner") {
                Owner::Held(held) => held,
                _ => unreachable!(),
            };
            held.revalidate_owner()?;
            held.writer().validate_tag_scope_in_connection(
                &observation,
                held.revision(),
                deadline,
            )?;
            custody
                .policy
                .as_ref()
                .expect("entered tag policy")
                .validate(&held.writer().database)?;
            if Instant::now() >= deadline {
                return Err(SourceWalPreparationError::Deadline);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(SourceTagWalPreparation {
                custody,
                observation,
                deadline,
            }),
            Err(error) => Err(SourceTagWalPreparationFailure { custody, error }),
        }
    }
}
impl SourceTagWalPreparation {
    pub(crate) fn original_wal_preparation(
        &self,
    ) -> Result<&super::SourceWalPreparation, SourceWalPreparationError> {
        self.custody.validate_parked()?;
        self.custody.held()
    }

    /// Frozen observations only, never a commit or journal decision grant.
    pub fn observation(&self) -> &SourceTagAdmissionObservation {
        &self.observation
    }
    /// Exact original source revision; an observation, not admission.
    pub fn revision(&self) -> &SourceRevision {
        self.observation.selection().revision()
    }
    /// Original cooperative deadline; expiry never proves rollback.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
    /// Check current ownership, policy and complete scope without opening a nested transaction.
    pub fn revalidate(&self) -> Result<(), SourceWalPreparationError> {
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        let held = match self.custody.owner.as_ref().expect("retained tag owner") {
            Owner::Held(held) => held,
            _ => unreachable!(),
        };
        held.revalidate_owner()?;
        self.custody
            .policy
            .as_ref()
            .expect("entered tag policy")
            .validate(&held.writer().database)?;
        held.writer().validate_tag_scope_in_connection(
            &self.observation,
            held.revision(),
            self.deadline,
        )?;
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        Ok(())
    }
    /// Return the exact writer only after rollback, timeout and checked policy restoration.
    pub fn abort(mut self) -> Result<SourceWriter, SourceTagWalPreparationFailure> {
        match self.custody.release() {
            Ok(writer) => Ok(writer),
            Err(error) => Err(SourceTagWalPreparationFailure {
                custody: self.custody,
                error,
            }),
        }
    }
}
impl SourceTagWalPreparationFailure {
    pub(crate) fn retained_database_identity(&self) -> Option<&str> {
        self.custody.retained_database_identity()
    }
    pub(crate) fn source_authority_close_ready(&self) -> bool {
        self.custody.source_authority_close_ready()
    }
    pub(crate) fn close_for_retirement(
        self,
        authority: &mut crate::sample_sources::db::SourceAuthorityRegistration,
    ) -> Result<
        crate::sample_sources::db::SourceAuthorityRetirementAck,
        crate::sample_sources::db::SourceAuthorityError,
    > {
        self.custody.close_for_retirement(authority)
    }

    /// Typed failure while the exact owner remains inaccessible.
    pub fn error(&self) -> &SourceWalPreparationError {
        if let Some(Owner::Failed(failed)) = self.custody.owner.as_ref() {
            failed.error()
        } else {
            &self.error
        }
    }
    /// Cleanup is explicit; sticky failed restoration never returns an apparently healthy owner.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        match self.custody.release() {
            Ok(writer) => Ok(writer),
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}
