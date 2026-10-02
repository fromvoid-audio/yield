//! Borrowed tag policy adapter; it never issues capacity or mutation authority.
use super::{
    SourceTagMetadataBudget, SourceWriter, SourceWriterError,
    metadata_policy_state::EnteredMetadataPolicy,
};
use crate::sample_sources::db::SourceDatabase;
/// SQLite settings retained on one source connection until transaction quiescence.
#[must_use = "finish only after the SQLite transaction has committed or rolled back"]
pub struct SourceTagMetadataPolicy<'a> {
    database: &'a SourceDatabase,
    state: EnteredMetadataPolicy,
}

impl SourceWriter {
    /// Enter a runtime restriction, never capacity or tag mutation authority.
    pub fn begin_tag_metadata_policy<'a>(
        &'a self,
        budget: &SourceTagMetadataBudget,
    ) -> Result<SourceTagMetadataPolicy<'a>, SourceWriterError> {
        if !self.database.connection.is_autocommit() {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        self.validate_tag_metadata_budget(budget)?;
        let policy = SourceTagMetadataPolicy {
            database: &self.database,
            state: EnteredMetadataPolicy::new(&self.database, budget.maximum_pages())?,
        };
        policy.state.install(&self.database)?;
        policy.validate_entered()?;
        if budget.revision() != &self.current_revision()? {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(policy)
    }
}

impl SourceTagMetadataPolicy<'_> {
    /// Revalidate the restriction; actual admission remains a separate requirement.
    pub fn validate_entered(&self) -> Result<(), SourceWriterError> {
        self.state.validate(self.database)
    }
    /// Restore only after confirmed transaction quiescence.
    pub fn finish(mut self) -> Result<(), SourceWriterError> {
        self.state
            .restore(self.database)
            .map_err(SourceWriterError::from)
    }
}
impl Drop for SourceTagMetadataPolicy<'_> {
    fn drop(&mut self) {
        if !self.state.finished && self.state.restore(self.database).is_err() {
            tracing::warn!(
                "source metadata policy restoration unconfirmed; database close/reopen required"
            );
        }
    }
}
