//! Private same-connection cleanup custody. No family validation or mutation authority.
use super::{
    SourceWalPreparation, SourceWalPreparationError, SourceWalPreparationFailure, SourceWriter,
    SourceWriterError, metadata_policy_state::EnteredMetadataPolicy,
};
// Owner drops before the extension guard: uncertain SQL closes before lease release.
pub(super) struct Custody {
    pub(super) owner: Option<Owner>,
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
