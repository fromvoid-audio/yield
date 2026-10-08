//! Permanent pre-open restriction for original existing MAIN preparation, never admission.
use super::*;

impl OwnedConnection {
    pub(crate) fn validate_original_legacy_read_fence(&self) -> bool {
        if !self.is_autocommit() || !self.has_no_effect_fence() {
            return false;
        }
        let Some(registration) = &self.registration else {
            return false;
        };
        let Ok(state) = registration.context.state.lock() else {
            return false;
        };
        if state.attempt.is_some()
            || state.open_auxiliary != 0
            || state.mapped_database != 0
            || state.mapped_shm_bytes != 0
        {
            return false;
        }
        drop(state);
        scalar(self, "query_only").ok() == Some(1)
            && scalar(self, "cache_spill").ok() == Some(0)
            && scalar(self, "temp_store").ok() == Some(2)
            && scalar(self, "mmap_size").ok() == Some(0)
    }
    pub(crate) fn has_no_effect_fence(&self) -> bool {
        self.registration
            .as_ref()
            .is_some_and(|r| r.context.no_effect.load(Ordering::Acquire))
    }
}

pub(crate) fn open_existing_no_effect(
    path: impl AsRef<std::path::Path>,
    descriptor: Option<std::sync::Arc<std::fs::File>>,
) -> rusqlite::Result<OwnedConnection> {
    if rusqlite::version_number() != 3_050_002 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    super::open_owned_mode(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        descriptor,
        true,
    )
}

// MMAP query/nonpositive setup is separately filtered in io::control. This helper rejects
// every other opcode outside the exact query/setup list before native xFileControl.
// PRAGMA is deliberately handled by SQLite core. No scalar enables writes through this fence.
pub(super) fn control_result(op: c_int) -> Option<c_int> {
    match op {
        ffi::SQLITE_FCNTL_PRAGMA => Some(ffi::SQLITE_NOTFOUND),
        ffi::SQLITE_FCNTL_HAS_MOVED
        | ffi::SQLITE_FCNTL_LOCKSTATE
        | ffi::SQLITE_FCNTL_LAST_ERRNO
        | ffi::SQLITE_FCNTL_VFSNAME
        | ffi::SQLITE_FCNTL_VFS_POINTER
        | ffi::SQLITE_FCNTL_FILE_POINTER
        | ffi::SQLITE_FCNTL_WIN32_GET_HANDLE
        | ffi::SQLITE_FCNTL_DATA_VERSION
        | ffi::SQLITE_FCNTL_BUSYHANDLER
        | ffi::SQLITE_FCNTL_TRACE => None,
        _ => Some(ffi::SQLITE_NOTFOUND),
    }
}

#[cfg(test)]
mod tests;

// Mutation authority is supplied only by the original retained committed journal consumer.
// Retain that borrow and the extension boundary until exact policy restoration or quarantine.
pub(crate) struct OriginalLegacyMutationScope<'a, 'owner> {
    connection: &'a OwnedConnection,
    _admission: OriginalLegacyAdmission<'a, 'owner>,
    _extensions: std::sync::RwLockReadGuard<'static, ()>,
    prior: [u64; 7],
    finished: bool,
}
enum OriginalLegacyAdmission<'a, 'owner> {
    Migration(&'a crate::operation_journal::owner::OriginalLegacyMigrationAdmission<'owner>),
    Bootstrap(&'a crate::operation_journal::owner::OriginalLegacyBootstrapAdmission<'owner>),
}
impl OriginalLegacyAdmission<'_, '_> {
    fn validate(
        &self,
        connection: &OwnedConnection,
    ) -> Result<(), crate::sample_sources::db::LegacyObservationError> {
        match self {
            Self::Migration(proof) => proof.validate_original_connection(connection),
            Self::Bootstrap(proof) => proof.validate_original_connection(connection),
        }
    }
    fn policy(&self) -> (u64, u64, u64, u64) {
        match self {
            Self::Migration(proof) => proof.source_policy(),
            Self::Bootstrap(proof) => proof.source_policy(),
        }
    }
}
fn scalar(connection: &Connection, name: &str) -> rusqlite::Result<u64> {
    connection.pragma_query_value(None, name, |row| row.get(0))
}
fn refused_scope() -> crate::sample_sources::db::LegacyObservationError {
    crate::sample_sources::db::LegacyObservationError("original migration policy unavailable")
}
impl OwnedConnection {
    pub(crate) fn enter_original_legacy_mutation<'a, 'owner>(
        &'a self,
        admission: &'a crate::operation_journal::owner::OriginalLegacyMigrationAdmission<'owner>,
    ) -> Result<
        OriginalLegacyMutationScope<'a, 'owner>,
        crate::sample_sources::db::LegacyObservationError,
    > {
        self.enter_original_legacy_scope(OriginalLegacyAdmission::Migration(admission))
    }
    pub(crate) fn enter_original_legacy_bootstrap<'a, 'owner>(
        &'a self,
        admission: &'a crate::operation_journal::owner::OriginalLegacyBootstrapAdmission<'owner>,
    ) -> Result<
        OriginalLegacyMutationScope<'a, 'owner>,
        crate::sample_sources::db::LegacyObservationError,
    > {
        self.enter_original_legacy_scope(OriginalLegacyAdmission::Bootstrap(admission))
    }
    fn enter_original_legacy_scope<'a, 'owner>(
        &'a self,
        admission: OriginalLegacyAdmission<'a, 'owner>,
    ) -> Result<
        OriginalLegacyMutationScope<'a, 'owner>,
        crate::sample_sources::db::LegacyObservationError,
    > {
        let extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or_else(refused_scope)?;
        admission.validate(self)?;
        let registration = self.registration.as_ref().ok_or_else(refused_scope)?;
        if !self.is_autocommit() || !self.has_no_effect_fence() {
            return Err(refused_scope());
        }
        {
            let state = registration
                .context
                .state
                .lock()
                .map_err(|_| refused_scope())?;
            if state.attempt.is_some()
                || state.open_auxiliary != 0
                || state.mapped_database != 0
                || state.mapped_shm_bytes != 0
            {
                return Err(refused_scope());
            }
        }
        let (page_size, original_max, selected_max, sector) = admission.policy();
        let mode: String = self
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .map_err(|_| refused_scope())?;
        let names = [
            "max_page_count",
            "cache_spill",
            "temp_store",
            "mmap_size",
            "wal_autocheckpoint",
            "synchronous",
            "fullfsync",
        ];
        let mut prior = [0; 7];
        for (index, name) in names.iter().enumerate() {
            prior[index] = scalar(self, name).map_err(|_| refused_scope())?;
        }
        if !mode.eq_ignore_ascii_case("delete")
            || scalar(self, "page_size").map_err(|_| refused_scope())? != page_size
            || self.known_delete_sector_size() != Some(sector)
            || prior[0] != original_max
            || prior[1] != 0
            || prior[2] != 2
            || prior[3] != 0
            || scalar(self, "query_only").map_err(|_| refused_scope())? != 1
            || selected_max > original_max
            || selected_max < scalar(self, "page_count").map_err(|_| refused_scope())?
            || selected_max
                .checked_mul(page_size)
                .is_none_or(|n| n > 64 * 1024 * 1024)
        {
            return Err(refused_scope());
        }
        let scope = OriginalLegacyMutationScope {
            connection: self,
            _admission: admission,
            _extensions: extensions,
            prior,
            finished: false,
        };
        // Core policy setup cannot write through the still-closed original VFS fence.
        self.pragma_update(None, "max_page_count", selected_max)
            .map_err(|_| refused_scope())?;
        self.execute_batch("PRAGMA cache_spill=OFF; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=0; PRAGMA wal_autocheckpoint=0; PRAGMA synchronous=EXTRA;")
            .map_err(|_| refused_scope())?;
        #[cfg(target_os = "macos")]
        self.execute_batch("PRAGMA fullfsync=ON")
            .map_err(|_| refused_scope())?;
        if scalar(self, "max_page_count").map_err(|_| refused_scope())? != selected_max
            || scalar(self, "cache_spill").map_err(|_| refused_scope())? != 0
            || scalar(self, "temp_store").map_err(|_| refused_scope())? != 2
            || scalar(self, "mmap_size").map_err(|_| refused_scope())? != 0
            || scalar(self, "wal_autocheckpoint").map_err(|_| refused_scope())? != 0
            || scalar(self, "synchronous").map_err(|_| refused_scope())? != 3
            || (cfg!(target_os = "macos")
                && scalar(self, "fullfsync").map_err(|_| refused_scope())? != 1)
        {
            return Err(refused_scope());
        }
        registration
            .context
            .no_effect
            .store(false, Ordering::Release);
        // This genuine original admission holds all physical claims. Disable inherited native
        // preallocation before query_only is cleared or any source transaction starts.
        let mut chunk: c_int = 0;
        // SAFETY: original sole-owner connection and live registration; SQLite's integer control.
        let rc = unsafe {
            ffi::sqlite3_file_control(
                self.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_CHUNK_SIZE,
                (&mut chunk as *mut c_int).cast(),
            )
        };
        if rc != ffi::SQLITE_OK {
            return Err(refused_scope());
        }
        self.execute_batch("PRAGMA query_only=OFF")
            .map_err(|_| refused_scope())?;
        if scalar(self, "query_only").map_err(|_| refused_scope())? != 0 {
            return Err(refused_scope());
        }
        Ok(scope)
    }
}
impl OriginalLegacyMutationScope<'_, '_> {
    pub(crate) fn finish(
        mut self,
    ) -> Result<(), crate::sample_sources::db::LegacyObservationError> {
        let result = self.restore();
        self.finished = true;
        result
    }
    fn restore(&self) -> Result<(), crate::sample_sources::db::LegacyObservationError> {
        let registration = self
            .connection
            .registration
            .as_ref()
            .ok_or_else(refused_scope)?;
        // Refence first: unconfirmed transactions must never perform rollback/cleanup in Drop.
        registration
            .context
            .no_effect
            .store(true, Ordering::Release);
        let restored = (|| -> rusqlite::Result<()> {
            if !self.connection.is_autocommit() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            self.connection.execute_batch("PRAGMA query_only=ON")?;
            for (name, value) in [
                "max_page_count",
                "cache_spill",
                "temp_store",
                "mmap_size",
                "wal_autocheckpoint",
                "synchronous",
                "fullfsync",
            ]
            .into_iter()
            .zip(self.prior)
            {
                self.connection.pragma_update(None, name, value)?;
                if scalar(self.connection, name)? != value {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            if scalar(self.connection, "query_only")? != 1 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(())
        })();
        if restored.is_err() {
            registration.context.quarantine();
            return Err(refused_scope());
        }
        Ok(())
    }
}
impl Drop for OriginalLegacyMutationScope<'_, '_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.restore();
        }
    }
}
