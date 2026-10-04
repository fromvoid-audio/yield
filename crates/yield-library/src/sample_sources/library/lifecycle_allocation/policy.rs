//! Private original global connection settings custody. A restriction is never admission.
use super::{LibraryDatabase, LifecycleAllocationError, LifecycleDatabaseAllocation, known_schema};
use rusqlite::Connection;
use std::{
    sync::RwLockReadGuard,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Settings {
    max_pages: u64,
    temp_store: u64,
    cache_spill: u64,
    auto_checkpoint: u64,
    mmap_size: u64,
}
impl Settings {
    fn read(connection: &Connection) -> Result<Self, LifecycleAllocationError> {
        let read = |name| {
            connection
                .pragma_query_value(None, name, |r| r.get(0))
                .map_err(|_| LifecycleAllocationError::Unknown)
        };
        Ok(Self {
            max_pages: read("max_page_count")?,
            temp_store: read("temp_store")?,
            cache_spill: read("cache_spill")?,
            auto_checkpoint: read("wal_autocheckpoint")?,
            mmap_size: read("mmap_size")?,
        })
    }
    fn install(&self, connection: &Connection) -> Result<(), LifecycleAllocationError> {
        for (name, value) in [
            ("max_page_count", self.max_pages),
            ("temp_store", self.temp_store),
            ("cache_spill", self.cache_spill),
            ("wal_autocheckpoint", self.auto_checkpoint),
            ("mmap_size", self.mmap_size),
        ] {
            connection
                .pragma_update(None, name, value)
                .map_err(|_| LifecycleAllocationError::Unknown)?;
        }
        if Self::read(connection)? != *self {
            return Err(LifecycleAllocationError::Unknown);
        }
        Ok(())
    }
}

pub(in crate::sample_sources::library) struct EnteredLifecyclePolicy<'a> {
    database: &'a LibraryDatabase,
    original: Settings,
    entered: Settings,
    allocation: LifecycleDatabaseAllocation,
    dirty: bool,
    finished: bool,
    _extensions: RwLockReadGuard<'static, ()>,
}
impl<'a> EnteredLifecyclePolicy<'a> {
    pub(in crate::sample_sources::library) fn new(
        database: &'a LibraryDatabase,
        deadline: Instant,
    ) -> Result<Self, LifecycleAllocationError> {
        if !database.connection.is_autocommit() {
            return Err(LifecycleAllocationError::Unknown);
        }
        let extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LifecycleAllocationError::Unknown)?;
        let allocation = database.capture_lifecycle_policy_input(deadline)?;
        let original = Settings::read(&database.connection)?;
        if original.max_pages != allocation.original_maximum_pages()
            || original.temp_store > 2
            || original.auto_checkpoint > i32::MAX as u64
            || original.cache_spill > i64::MAX as u64
            || original.mmap_size > i64::MAX as u64
        {
            return Err(LifecycleAllocationError::Unknown);
        }
        let entered = Settings {
            max_pages: allocation.selected_maximum_pages(),
            temp_store: 2,
            cache_spill: 0,
            auto_checkpoint: 0,
            mmap_size: 0,
        };
        Ok(Self {
            database,
            original,
            entered,
            allocation,
            dirty: false,
            finished: false,
            _extensions: extensions,
        })
    }
    pub(in crate::sample_sources::library) fn install(
        &mut self,
    ) -> Result<(), LifecycleAllocationError> {
        if self.finished || self.dirty || !self.database.connection.is_autocommit() {
            return Err(LifecycleAllocationError::Unknown);
        }
        // Record partial installation before the first potentially failing setting update.
        self.dirty = true;
        self.entered.install(&self.database.connection)
    }
    pub(in crate::sample_sources::library) fn validate_settings(
        &self,
    ) -> Result<(), LifecycleAllocationError> {
        if self.finished
            || !self.dirty
            || Settings::read(&self.database.connection)? != self.entered
        {
            return Err(LifecycleAllocationError::Unknown);
        }
        Ok(())
    }
    pub(in crate::sample_sources::library) fn validate_reserved(
        &self,
        deadline: Instant,
    ) -> Result<(), LifecycleAllocationError> {
        self.validate_settings()?;
        let actual = self.database.capture_lifecycle_allocation(deadline)?;
        if !self.allocation.matches_entered(&actual) {
            return Err(LifecycleAllocationError::Unknown);
        }
        Ok(())
    }
    pub(in crate::sample_sources::library) fn allocation(&self) -> &LifecycleDatabaseAllocation {
        &self.allocation
    }
    pub(in crate::sample_sources::library) fn finish(
        &mut self,
    ) -> Result<(), LifecycleAllocationError> {
        if self.finished {
            return Err(LifecycleAllocationError::Unknown);
        }
        self.finished = true;
        if !self.dirty {
            return Ok(());
        }
        // Expired preparation cannot extend its reservation. Bounded cleanup is separate.
        let restored = self.database.connection.is_autocommit()
            && super::lifecycle_allocation::bounded_read(
                &self.database.connection,
                Instant::now() + Duration::from_secs(2),
                || {
                    known_schema(self.database)?;
                    self.original.install(&self.database.connection)
                },
            )
            .is_ok();
        if restored {
            self.dirty = false;
            Ok(())
        } else {
            let _ = self
                .database
                .connection
                .pragma_update(None, "query_only", true);
            Err(LifecycleAllocationError::Unknown)
        }
    }
}
impl Drop for EnteredLifecyclePolicy<'_> {
    fn drop(&mut self) {
        if !self.finished && self.dirty {
            // Unwind keeps the original connection borrowed through confirmed empty cleanup.
            if !self.database.connection.is_autocommit() {
                let _ = self.database.connection.execute_batch("ROLLBACK");
            }
            let _ = self.finish();
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[cfg(target_os = "linux")]
mod workload_tests;
