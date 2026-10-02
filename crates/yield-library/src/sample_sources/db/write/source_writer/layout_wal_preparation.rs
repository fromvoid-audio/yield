//! Owned pre-intent layout allocation policy and empty reservation; no mutation/claim authority.
use super::{
    SourceLayoutMetadataBudget, SourceRevision, SourceWalPreparationError, SourceWriter,
    metadata_policy_state::EnteredMetadataPolicy,
    metadata_wal_custody::{Custody, Owner},
    wal_reservation,
};
use std::{fmt, time::Instant};

/// Same-connection layout allocation policy custody and exact frozen allocation facts, never admission.
/// This value stays on its owning thread because it retains the extension-policy read guard.
pub struct SourceLayoutWalPreparation {
    custody: Custody,
    budget: SourceLayoutMetadataBudget,
    deadline: Instant,
}

/// A rejected preparation keeps its genuine owner until cleanup is confirmed.
pub struct SourceLayoutWalPreparationFailure {
    custody: Custody,
    error: SourceWalPreparationError,
}

impl fmt::Debug for SourceLayoutWalPreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceLayoutWalPreparation")
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SourceLayoutWalPreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceLayoutWalPreparationFailure")
            .field("error", &self.error())
            .finish()
    }
}

impl SourceWriter {
    /// Retain the original layout budget, settings and empty immediate transaction.
    /// No intent, capacity claim, source/control mutation or layout publication is admitted.
    pub fn prepare_layout_wal_reservation(
        self,
        budget: SourceLayoutMetadataBudget,
        requested_deadline: Instant,
    ) -> Result<SourceLayoutWalPreparation, SourceLayoutWalPreparationFailure> {
        let deadline = requested_deadline.min(budget.observation_deadline());
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
            let revision = wal_reservation::validate(custody.writer(), budget.revision(), deadline);
            custody.track_created_transaction();
            let revision = revision?;
            custody
                .writer()
                .validate_layout_metadata_facts(&budget, &revision, deadline)?;
            custody.policy = Some(EnteredMetadataPolicy::new(
                &custody.writer().database,
                budget.maximum_pages(),
            )?);
            custody
                .policy
                .as_ref()
                .expect("entered layout allocation policy")
                .install(&custody.writer().database)?;
            custody
                .policy
                .as_ref()
                .expect("entered layout allocation policy")
                .validate(&custody.writer().database)?;
            let writer = match custody.owner.take().expect("retained layout owner") {
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
            let held = match custody.owner.as_ref().expect("retained layout owner") {
                Owner::Held(held) => held,
                _ => unreachable!(),
            };
            held.revalidate_owner()?;
            held.writer()
                .validate_layout_metadata_facts(&budget, held.revision(), deadline)?;
            custody
                .policy
                .as_ref()
                .expect("entered layout allocation policy")
                .validate(&held.writer().database)?;
            if Instant::now() >= deadline {
                return Err(SourceWalPreparationError::Deadline);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(SourceLayoutWalPreparation {
                custody,
                budget,
                deadline,
            }),
            Err(error) => Err(SourceLayoutWalPreparationFailure { custody, error }),
        }
    }
}
impl SourceLayoutWalPreparation {
    /// Original allocation observations only, never a commit or journal decision grant.
    pub fn budget(&self) -> &SourceLayoutMetadataBudget {
        &self.budget
    }
    /// Exact original source revision; an allocation observation, never admission.
    pub fn revision(&self) -> &SourceRevision {
        self.budget.revision()
    }
    /// Original cooperative deadline; expiry never proves rollback.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
    /// Check current ownership, policy and original allocation facts without opening a nested transaction.
    pub fn revalidate(&self) -> Result<(), SourceWalPreparationError> {
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        let held = match self.custody.owner.as_ref().expect("retained layout owner") {
            Owner::Held(held) => held,
            _ => unreachable!(),
        };
        held.revalidate_owner()?;
        self.custody
            .policy
            .as_ref()
            .expect("entered layout allocation policy")
            .validate(&held.writer().database)?;
        held.writer().validate_layout_metadata_facts(
            &self.budget,
            held.revision(),
            self.deadline,
        )?;
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        Ok(())
    }
    /// Return the exact writer only after rollback, timeout and checked policy restoration.
    pub fn abort(mut self) -> Result<SourceWriter, SourceLayoutWalPreparationFailure> {
        match self.custody.release() {
            Ok(writer) => Ok(writer),
            Err(error) => Err(SourceLayoutWalPreparationFailure {
                custody: self.custody,
                error,
            }),
        }
    }
}
impl SourceLayoutWalPreparationFailure {
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
