//! Private finite SQLite restriction state shared by metadata families, never admission.
use super::SourceWriterError;
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

// Owns settings and the extension gate, never a pointer/reference into its moving writer.
// The borrowing adapter and owned preparation both restore through the same state machine.
pub(super) struct EnteredMetadataPolicy {
    original: Settings,
    entered: Settings,
    pub(super) finished: bool,
    _extensions: RwLockReadGuard<'static, ()>,
}

impl EnteredMetadataPolicy {
    pub(super) fn new(
        database: &SourceDatabase,
        maximum_pages: u64,
    ) -> Result<Self, SourceWriterError> {
        if !database.connection.is_autocommit()
            || database.tag_metadata_policy_state.get() != TAG_METADATA_POLICY_IDLE
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        let extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(SourceWriterError::MetadataBoundNotDefined)?;
        schema::require_known_metadata_schema(&database.connection)
            .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?;
        let original = Settings::read(&database.connection)?;
        let query_only: bool = database
            .connection
            .pragma_query_value(None, "query_only", |r| r.get(0))
            .map_err(map_sql_error)?;
        if query_only
            || original.temp_store > 2
            || original.auto_checkpoint > i32::MAX as u64
            || original.max_pages < maximum_pages
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        let entered = Settings {
            max_pages: maximum_pages,
            temp_store: 2,
            cache_spill: 0,
            auto_checkpoint: 0,
        };
        let state = Self {
            original,
            entered,
            finished: false,
            _extensions: extensions,
        };

        Ok(state)
    }

    pub(super) fn install(&self, database: &SourceDatabase) -> Result<(), SourceWriterError> {
        database
            .tag_metadata_policy_state
            .set(TAG_METADATA_POLICY_ENTERED);
        self.entered
            .install(&database.connection)
            .map_err(SourceWriterError::from)
    }

    pub(super) fn validate(&self, database: &SourceDatabase) -> Result<(), SourceWriterError> {
        database.validate_profile_owned_mutation()?;
        if self.finished
            || database.tag_metadata_policy_state.get() != TAG_METADATA_POLICY_ENTERED
            || Settings::read(&database.connection)? != self.entered
        {
            return Err(SourceWriterError::MetadataBoundNotDefined);
        }
        schema::require_known_metadata_schema(&database.connection)
            .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?;
        let page_size: u64 = database
            .connection
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .map_err(map_sql_error)?;
        let page_count: u64 = database
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

    pub(super) fn restore(&mut self, database: &SourceDatabase) -> Result<(), SourceDbError> {
        if self.finished {
            return Err(SourceDbError::MetadataPolicyUnconfirmed);
        }
        self.finished = true;
        let connection = &database.connection;
        let restored = connection.is_autocommit()
            && schema::require_known_metadata_schema(connection).is_ok()
            && self.original.install(connection).is_ok();
        if restored {
            database
                .tag_metadata_policy_state
                .set(TAG_METADATA_POLICY_IDLE);
            Ok(())
        } else {
            database
                .tag_metadata_policy_state
                .set(TAG_METADATA_POLICY_POISONED);
            let _ = connection.pragma_update(None, "query_only", true);
            Err(SourceDbError::MetadataPolicyUnconfirmed)
        }
    }
}
