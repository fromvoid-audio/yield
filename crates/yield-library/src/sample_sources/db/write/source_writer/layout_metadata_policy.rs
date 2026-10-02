//! Borrowed layout policy adapter; no admission or publication authority.
use super::{
    SourceLayoutMetadataBudget, SourceWriter, SourceWriterError,
    metadata_policy_state::EnteredMetadataPolicy,
};
use crate::sample_sources::db::SourceDatabase;
/// SQLite settings retained on one source connection until its transaction is quiescent.
///
/// This is neither a capacity claim nor a layout publication grant. The actual participant must
/// obtain genuine owner admission before entering, retain that admission through its transaction,
/// and reconcile its immutable outcome if policy restoration returns an error after commit.
/// This standalone guard is not the combined WAL-reservation/admission implementation.
/// The eventual admitted layout participant must compose those scopes on the same connection.
#[must_use = "finish only after the SQLite transaction has committed or rolled back"]
pub struct SourceLayoutMetadataPolicy<'a> {
    database: &'a SourceDatabase,
    state: EnteredMetadataPolicy,
    deadline: std::time::Instant,
    revision: u64,
}
impl SourceWriter {
    /// Revalidate a real observation and enter the capped SQLite runtime policy.
    /// This configures the existing connection, without granting writes or a journal ID.
    pub fn begin_layout_metadata_policy<'a>(
        &'a self,
        budget: &SourceLayoutMetadataBudget,
    ) -> Result<SourceLayoutMetadataPolicy<'a>, SourceWriterError> {
        if !self.database.connection.is_autocommit() {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        self.validate_layout_metadata_budget(budget)?;
        let mut policy = SourceLayoutMetadataPolicy {
            database: &self.database,
            state: EnteredMetadataPolicy::new(&self.database, budget.maximum_pages())?,
            deadline: budget.observation_deadline(),
            revision: budget.revision().revision(),
        };
        if let Err(error) = policy.state.install(&self.database) {
            policy.state.restore(&self.database)?;
            return Err(error);
        }
        policy.validate_entered()?;
        if budget.revision() != &self.current_revision()? {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(policy)
    }
}
impl SourceLayoutMetadataPolicy<'_> {
    /// Check the actual finite restriction; genuine admission and logical scope remain required.
    pub fn validate_entered(&self) -> Result<(), SourceWriterError> {
        if std::time::Instant::now() >= self.deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        self.state.validate(self.database)?;
        if self.database.get_revision()? != self.revision {
            return Err(SourceWriterError::StaleFence);
        }
        super::layout_metadata_budget::generation_observation(&self.database.connection)?;
        Ok(())
    }
    /// Restore only after confirmed quiescence; failed restoration preserves poisoned custody.
    pub fn finish(mut self) -> Result<(), SourceWriterError> {
        self.state
            .restore(self.database)
            .map_err(SourceWriterError::from)
    }
}
impl Drop for SourceLayoutMetadataPolicy<'_> {
    fn drop(&mut self) {
        if !self.state.finished && self.state.restore(self.database).is_err() {
            tracing::warn!(
                "source metadata policy restoration unconfirmed; database close/reopen required"
            );
        }
    }
}
