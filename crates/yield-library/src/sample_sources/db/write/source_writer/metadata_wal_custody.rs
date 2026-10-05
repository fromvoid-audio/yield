//! Private same-connection cleanup custody. No family validation or mutation authority.
use super::{
    SourceWalPreparation, SourceWalPreparationError, SourceWalPreparationFailure, SourceWriter,
    SourceWriterError, metadata_policy_state::EnteredMetadataPolicy,
};
// Owner drops before the extension guard: uncertain SQL closes before lease release.
pub(super) struct Custody {
    pub(super) owner: Option<Owner>,
    // Moves out of the inner reservation before cleanup, keeping the count through
    // outer policy restoration. Field order closes owner before this counted use/lease.
    pub(super) registered:
        Option<crate::sample_sources::db::source_authority::RegisteredDatabaseCustody>,
    pub(super) policy: Option<EnteredMetadataPolicy>,
}
pub(super) enum Owner {
    Unprepared {
        writer: SourceWriter,
        created_transaction: bool,
    },
    Held(SourceWalPreparation),
    Failed(SourceWalPreparationFailure),
}
impl Custody {
    pub(super) fn held(&self) -> Result<&SourceWalPreparation, SourceWalPreparationError> {
        match self.owner.as_ref() {
            Some(Owner::Held(held)) => Ok(held),
            _ => Err(SourceWalPreparationError::RollbackUnconfirmed),
        }
    }
    pub(super) fn validate_parked(&self) -> Result<(), SourceWalPreparationError> {
        let held = self.held()?;
        held.revalidate_owner()?;
        self.policy
            .as_ref()
            .ok_or(SourceWalPreparationError::RollbackUnconfirmed)?
            .validate_parked(&held.writer().database)?;
        Ok(())
    }
    pub(super) fn retained_database_identity(&self) -> Option<&str> {
        self.writer()
            .database
            .database_file_binding
            .as_deref()
            .map(|b| b.identity())
    }
    pub(super) fn source_authority_close_ready(&self) -> bool {
        if let Some(custody) = self.registered.as_ref() {
            return custody.close_ready(&self.writer().database);
        }
        match self.owner.as_ref().expect("retained policy owner") {
            Owner::Held(held) => held.source_authority_close_ready(),
            Owner::Failed(failed) => failed.source_authority_close_ready(),
            Owner::Unprepared { writer, .. } => writer.database.source_authority_close_ready(),
        }
    }
    pub(super) fn retain_registered(
        &mut self,
        binding: &super::SourceWalBinding,
        registration: &crate::sample_sources::db::SourceAuthorityRegistration,
    ) -> Result<(), SourceWalPreparationError> {
        self.validate_parked()?;
        if self.registered.is_some() {
            return Err(SourceWalPreparationError::BindingUnavailable);
        }
        match self.owner.as_mut() {
            Some(Owner::Held(held)) => held.retain_registered_wal_binding(binding, registration),
            _ => Err(SourceWalPreparationError::RollbackUnconfirmed),
        }
    }
    pub(super) fn close_for_retirement(
        mut self,
        authority: &mut crate::sample_sources::db::SourceAuthorityRegistration,
    ) -> Result<
        crate::sample_sources::db::SourceAuthorityRetirementAck,
        crate::sample_sources::db::SourceAuthorityError,
    > {
        use crate::sample_sources::db::SourceAuthorityError;
        match self.owner.take().expect("original policy custody") {
            Owner::Unprepared {
                writer,
                created_transaction,
            } => {
                if !created_transaction && !writer.database.connection.is_autocommit() {
                    self.owner = Some(Owner::Unprepared {
                        writer,
                        created_transaction,
                    });
                    return Err(SourceAuthorityError::InvalidLifecycle);
                }
                authority.close_database_with_registered_custody(
                    writer.into_catalog_database(),
                    self.registered.take(),
                )
            }
            Owner::Held(held) => {
                held.close_for_retirement_with_custody(authority, self.registered.take())
            }
            Owner::Failed(failed) => {
                failed.close_for_retirement_with_custody(authority, self.registered.take())
            }
        }
    }

    pub(super) fn writer(&self) -> &SourceWriter {
        match self.owner.as_ref().expect("retained tag owner") {
            Owner::Unprepared { writer, .. } => writer,
            Owner::Held(held) => held.writer(),
            Owner::Failed(failed) => failed.writer(),
        }
    }
    pub(super) fn track_created_transaction(&mut self) {
        if let Some(Owner::Unprepared {
            writer,
            created_transaction,
        }) = self.owner.as_mut()
        {
            *created_transaction |= !writer.database.connection.is_autocommit();
        }
    }
    pub(super) fn release(&mut self) -> Result<SourceWriter, SourceWalPreparationError> {
        let mut owner = self.owner.take().expect("retained tag owner");
        // Move, never clone or release, the original count before inner rollback/timeout.
        let retained = match &mut owner {
            Owner::Held(held) => held.take_registered_database_custody(),
            Owner::Failed(failed) => failed.take_registered_database_custody(),
            Owner::Unprepared { .. } => None,
        };
        if let Some(retained) = retained {
            debug_assert!(self.registered.is_none(), "one original registered custody");
            self.registered = Some(retained);
        }
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
        // Original inner cleanup and all outer policy restoration are confirmed.
        drop(self.registered.take());
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
