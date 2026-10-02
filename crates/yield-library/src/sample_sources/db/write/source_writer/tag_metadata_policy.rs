//! Entered runtime allocation policy; this guard never issues capacity or mutation authority.
use super::{SourceTagMetadataBudget, SourceWriter, SourceWriterError};
use crate::sample_sources::db::{
    SourceDatabase, SourceDbError, TAG_METADATA_POLICY_ENTERED, TAG_METADATA_POLICY_IDLE,
    TAG_METADATA_POLICY_POISONED, schema, util::map_sql_error,
};
use rusqlite::Connection;
use std::sync::RwLockReadGuard;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Settings {
    max_pages: u64,
    temp_store: u64,
    cache_spill: u64,
    auto_checkpoint: u64,
}
impl Settings {
    fn read(connection: &Connection) -> Result<Self, SourceDbError> {
        let read = |name| {
            connection
                .pragma_query_value(None, name, |row| row.get(0))
                .map_err(map_sql_error)
        };
        Ok(Self {
            max_pages: read("max_page_count")?,
            temp_store: read("temp_store")?,
            cache_spill: read("cache_spill")?,
            auto_checkpoint: read("wal_autocheckpoint")?,
        })
    }
    fn install(&self, connection: &Connection) -> Result<(), SourceDbError> {
        for (name, value) in [
            ("max_page_count", self.max_pages),
            ("temp_store", self.temp_store),
            ("cache_spill", self.cache_spill),
            ("wal_autocheckpoint", self.auto_checkpoint),
        ] {
            connection
                .pragma_update(None, name, value)
                .map_err(map_sql_error)?;
        }
        if Self::read(connection)? != *self {
            return Err(SourceDbError::MetadataPolicyUnconfirmed);
        }
        Ok(())
    }
}

/// SQLite settings retained on one source connection until its transaction is quiescent.
///
/// This is neither a capacity claim nor a tag mutation grant. The actual participant must
/// obtain genuine owner admission before entering, retain that admission through its transaction,
/// and reconcile its immutable outcome if policy restoration returns an error after commit.
#[must_use = "finish only after the SQLite transaction has committed or rolled back"]
pub struct SourceTagMetadataPolicy<'a> {
    database: &'a SourceDatabase,
    original: Settings,
    entered: Settings,
    finished: bool,
    // Kept on this owner thread until transaction quiescence and checked restoration.
    _extensions: RwLockReadGuard<'static, ()>,
}

impl SourceWriter {
    /// Revalidate a real observation and enter the capped SQLite runtime policy.
    /// This configures this existing connection; it does not grant writes or issue a journal ID.
    pub fn begin_tag_metadata_policy<'a>(
        &'a self,
        budget: &SourceTagMetadataBudget,
    ) -> Result<SourceTagMetadataPolicy<'a>, SourceWriterError> {
        if !self.database.connection.is_autocommit() {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        self.validate_tag_metadata_budget(budget)?;
        let extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(SourceWriterError::MetadataBoundNotDefined)?;
        schema::require_known_metadata_schema(&self.database.connection)
            .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?;
        let original = Settings::read(&self.database.connection)?;
        let query_only: bool = self
            .database
            .connection
            .pragma_query_value(None, "query_only", |r| r.get(0))
            .map_err(map_sql_error)?;
        if query_only
            || original.temp_store > 2
            || original.auto_checkpoint > i32::MAX as u64
            || original.max_pages < budget.maximum_pages()
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        let entered = Settings {
            max_pages: budget.maximum_pages(),
            temp_store: 2,
            cache_spill: 0,
            auto_checkpoint: 0,
        };
        let mut policy = SourceTagMetadataPolicy {
            database: &self.database,
            original,
            entered,
            finished: false,
            _extensions: extensions,
        };
        self.database
            .tag_metadata_policy_state
            .set(TAG_METADATA_POLICY_ENTERED);
        if let Err(error) = entered.install(&self.database.connection) {
            // Partial installation is not an entered grant. Failed cleanup poisons the incarnation.
            policy.restore()?;
            return Err(error.into());
        }
        policy.validate_entered()?;
        if budget.revision() != &self.current_revision()? {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(policy)
    }
}

impl SourceTagMetadataPolicy<'_> {
    /// Check the finite runtime policy immediately before the actual participant's effect.
    /// Logical selection, current source authority and genuine capacity claims remain its duty.
    pub fn validate_entered(&self) -> Result<(), SourceWriterError> {
        self.database.validate_profile_owned_mutation()?;
        if self.finished
            || self.database.tag_metadata_policy_state.get() != TAG_METADATA_POLICY_ENTERED
            || Settings::read(&self.database.connection)? != self.entered
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        schema::require_known_metadata_schema(&self.database.connection)
            .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?;
        let page_size: u64 = self
            .database
            .connection
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .map_err(map_sql_error)?;
        let page_count: u64 = self
            .database
            .connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .map_err(map_sql_error)?;
        if page_count > self.entered.max_pages
            || page_size
                .checked_mul(self.entered.max_pages)
                .is_none_or(|v| v > 64 * 1024 * 1024)
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        Ok(())
    }

    /// Restore only after confirmed transaction quiescence. An error preserves poisoned state;
    /// it must never turn an uncertain tag commit into a successful completion.
    pub fn finish(mut self) -> Result<(), SourceWriterError> {
        self.restore().map_err(SourceWriterError::from)
    }

    fn restore(&mut self) -> Result<(), SourceDbError> {
        if self.finished {
            return Err(SourceDbError::MetadataPolicyUnconfirmed);
        }
        self.finished = true;
        let connection = &self.database.connection;
        let restored = connection.is_autocommit()
            && schema::require_known_metadata_schema(connection).is_ok()
            && self.original.install(connection).is_ok();
        if restored {
            self.database
                .tag_metadata_policy_state
                .set(TAG_METADATA_POLICY_IDLE);
            Ok(())
        } else {
            self.database
                .tag_metadata_policy_state
                .set(TAG_METADATA_POLICY_POISONED);
            // Defense in depth for internal raw SQLite access; the owned admission hook also
            // rejects. This does not roll back or claim completion of an unfinished transaction.
            let _ = connection.pragma_update(None, "query_only", true);
            Err(SourceDbError::MetadataPolicyUnconfirmed)
        }
    }
}

impl Drop for SourceTagMetadataPolicy<'_> {
    fn drop(&mut self) {
        if !self.finished && self.restore().is_err() {
            tracing::warn!(
                "source metadata policy restoration unconfirmed; database close/reopen required"
            );
        }
    }
}
