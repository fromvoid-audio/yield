//! Owned pre-intent tag policy and empty reservation; no mutation/claim authority.
use super::{
    SourceRevision, SourceTagAdmissionObservation, SourceWalPreparation, SourceWalPreparationError,
    SourceWalPreparationFailure, SourceWriter, SourceWriterError,
    tag_metadata_policy::EnteredTagPolicy, wal_reservation,
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

// Owner drops before the extension guard: uncertain SQL closes before lease release.
struct Custody {
    owner: Option<Owner>,
    policy: Option<EnteredTagPolicy>,
}
enum Owner {
    Unprepared {
        writer: SourceWriter,
        created_transaction: bool,
    },
    Held(SourceWalPreparation),
    Failed(SourceWalPreparationFailure),
}
impl Custody {
    fn writer(&self) -> &SourceWriter {
        match self.owner.as_ref().expect("retained tag owner") {
            Owner::Unprepared { writer, .. } => writer,
            Owner::Held(held) => held.writer(),
            Owner::Failed(failed) => failed.writer(),
        }
    }
    fn track_created_transaction(&mut self) {
        if let Some(Owner::Unprepared {
            writer,
            created_transaction,
        }) = self.owner.as_mut()
        {
            *created_transaction |= !writer.database.connection.is_autocommit();
        }
    }
    fn release(&mut self) -> Result<SourceWriter, SourceWalPreparationError> {
        let owner = self.owner.take().expect("retained tag owner");
        let writer = match owner {
            Owner::Unprepared {
                writer,
                created_transaction,
            } => {
                // Never adopt a transaction that predates this preparation, and never
                // return an active connection to ordinary dispatch. Retain it until close.
                if !created_transaction && !writer.database.connection.is_autocommit() {
                    self.owner = Some(Owner::Unprepared {
                        writer,
                        created_transaction,
                    });
                    return Err(SourceWalPreparationError::TransactionActive);
                }
                if created_transaction
                    && (writer
                        .database
                        .connection
                        .execute_batch("ROLLBACK")
                        .is_err()
                        || !writer.database.connection.is_autocommit())
                {
                    self.owner = Some(Owner::Unprepared {
                        writer,
                        created_transaction,
                    });
                    return Err(SourceWalPreparationError::RollbackUnconfirmed);
                }
                writer
            }
            Owner::Held(held) => match held.abort() {
                Ok(writer) => writer,
                Err(failed) => {
                    self.owner = Some(Owner::Failed(failed));
                    return Err(SourceWalPreparationError::RollbackUnconfirmed);
                }
            },
            Owner::Failed(failed) => match failed.into_writer() {
                Ok(writer) => writer,
                Err(failed) => {
                    self.owner = Some(Owner::Failed(failed));
                    return Err(SourceWalPreparationError::RollbackUnconfirmed);
                }
            },
        };
        self.owner = Some(Owner::Unprepared {
            writer,
            created_transaction: false,
        });
        if self.policy.is_none()
            && self.writer().database.tag_metadata_policy_state.get()
                == crate::sample_sources::db::TAG_METADATA_POLICY_POISONED
        {
            return Err(SourceWriterError::from(
                crate::sample_sources::db::SourceDbError::MetadataPolicyUnconfirmed,
            )
            .into());
        }
        if let Some(policy) = self.policy.as_mut() {
            let database = match self.owner.as_ref().expect("retained tag owner") {
                Owner::Unprepared { writer, .. } => &writer.database,
                _ => unreachable!(),
            };
            policy.restore(database).map_err(SourceWriterError::from)?;
        }
        self.policy = None;
        match self.owner.take().expect("retained tag owner") {
            Owner::Unprepared { writer, .. } => Ok(writer),
            _ => unreachable!(),
        }
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        if self.owner.is_some() {
            // Failure leaves the owner inaccessible. Its connection closes before lease release;
            // neither a failed rollback nor restoration can return it to ordinary dispatch.
            let _ = self.release();
        }
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
            custody.policy = Some(EnteredTagPolicy::new(
                &custody.writer().database,
                observation.budget(),
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
