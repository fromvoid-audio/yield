//! Historical journal/profile facts for legacy preparation, never an entered policy or grant.
use super::{JOURNAL_DATABASE_FILE_NAME, JOURNAL_SCHEMA_VERSION, OperationJournalStore};
use crate::{app_dirs::WritableProfileGuard, durable_capacity::VolumeIdentity};
use rusqlite::Connection;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rusqlite::ffi;
use std::{
    path::Path,
    time::{Duration, Instant},
};

const CAP: u64 = 512 * 1024 * 1024;
/// Refusal to observe an unsupported, stale or unbounded genuine journal owner.
#[derive(Debug, thiserror::Error)]
#[error("legacy journal observation refused: {0}")]
pub struct LegacyJournalObservationError(&'static str);
type Result<T> = std::result::Result<T, LegacyJournalObservationError>;
fn refuse(_: impl std::fmt::Debug) -> LegacyJournalObservationError {
    LegacyJournalObservationError("SQLite, namespace or descriptor validation failed")
}

/// Actual original owner facts plus a conditional future non-spilling DELETE estimate.
/// It cannot be deserialized, constructed by a caller, or used as admission authority.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacyJournalObservation {
    database_identity: String,
    profile_identity: String,
    lock_identity: String,
    owner_identity: String,
    volume: VolumeIdentity,
    unit: u64,
    directory_identity: String,
    sidecar_volume: VolumeIdentity,
    sidecar_unit: u64,
    main_bytes: u64,
    rollback_bytes: u64,
    rollback_identity: Option<String>,
    page_size: u64,
    page_count: u64,
    current_max_pages: u64,
    selected_max_pages: u64,
    sector: u64,
    cache_spill: u64,
    temp_store: u64,
    lock_bytes: u64,
    lock_volume: VolumeIdentity,
    lock_unit: u64,
    database_bound: u64,
    rollback_bound: u64,
    peak: u64,
}
impl LegacyJournalObservation {
    /// Original retained main-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }
    /// Original retained profile-root identity.
    pub fn profile_identity(&self) -> &str {
        &self.profile_identity
    }
    /// Original acquired profile-owner identity; no ownership grant is recreated.
    pub fn owner_identity(&self) -> &str {
        &self.owner_identity
    }
    /// Original retained profile lock identity.
    pub fn profile_lock_identity(&self) -> &str {
        &self.lock_identity
    }
    /// Actual descriptor volume, without free capacity or registration authority.
    pub fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    /// Original retained database-directory identity, distinct from its main file.
    pub fn directory_identity(&self) -> &str {
        &self.directory_identity
    }
    /// Actual retained directory volume for DELETE sidecar creation.
    pub fn sidecar_volume(&self) -> &VolumeIdentity {
        &self.sidecar_volume
    }
    /// Actual directory filesystem unit for sidecar allowance observations.
    pub fn sidecar_allocation_unit(&self) -> u64 {
        self.sidecar_unit
    }
    /// Actual filesystem unit observation, not a qualified physical peak.
    pub fn allocation_unit(&self) -> u64 {
        self.unit
    }
    /// Main length from SQLite's retained VFS.
    pub fn main_bytes(&self) -> u64 {
        self.main_bytes
    }
    /// Observed ordinary DELETE sidecar bytes, including retained zero-header evidence.
    pub fn existing_rollback_bytes(&self) -> u64 {
        self.rollback_bytes
    }
    /// Observed sidecar identity; absence is not a mutation permit.
    pub fn existing_rollback_identity(&self) -> Option<&str> {
        self.rollback_identity.as_deref()
    }
    /// Actual SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Actual current page count.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Current cap, never raised by observation.
    pub fn current_max_pages(&self) -> u64 {
        self.current_max_pages
    }
    /// Proposed cap, at most the current cap and 512MiB/page_size.
    pub fn selected_max_pages(&self) -> u64 {
        self.selected_max_pages
    }
    /// Actual retained known native VFS sector callback result.
    pub fn sector_size(&self) -> u64 {
        self.sector
    }
    /// Actual connection setting, not silently disabled during capture.
    pub fn current_cache_spill(&self) -> u64 {
        self.cache_spill
    }
    /// Actual connection temporary-store setting, not silently switched during capture.
    pub fn current_temp_store(&self) -> u64 {
        self.temp_store
    }
    /// Existing original profile lock bytes; capture writes no profile control bytes.
    pub fn profile_lock_bytes(&self) -> u64 {
        self.lock_bytes
    }
    /// Actual profile lock descriptor volume; aliases must be deduplicated by the consumer.
    pub fn profile_lock_volume(&self) -> &VolumeIdentity {
        &self.lock_volume
    }
    /// Actual original profile lock filesystem unit.
    pub fn profile_lock_allocation_unit(&self) -> u64 {
        self.lock_unit
    }
    /// Proposed complete journal database allowance, including every schema/index/record family.
    pub fn proposed_database_bytes(&self) -> u64 {
        self.database_bound
    }
    /// Conditional DELETE rollback allowance; no savepoints, spills or attached databases.
    pub fn proposed_rollback_bytes(&self) -> u64 {
        self.rollback_bound
    }
    /// Conditional whole-journal logical peak. Future admission must enter and verify the cap,
    /// cache_spill=OFF and temp_store=MEMORY, exclude extensions and disallow savepoints/attachments.
    /// This value is not physical allocation, a live capacity claim or entered policy.
    pub fn proposed_journal_logical_peak_bytes(&self) -> u64 {
        self.peak
    }
}
impl OperationJournalStore {
    /// Read through the genuine retained journal/profile owner without changing any settings or
    /// rows. No second SQLite connection, operation ID, intent, claim or receipt is created.
    pub fn capture_legacy_journal_observation(
        &self,
        profile: &WritableProfileGuard,
    ) -> Result<LegacyJournalObservation> {
        self.capture_legacy_journal_until(profile, Instant::now() + Duration::from_secs(2))
    }
    pub(crate) fn capture_legacy_journal_until(
        &self,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<LegacyJournalObservation> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope().ok_or(
            LegacyJournalObservationError("SQLite extension runtime unknown"),
        )?;
        if Instant::now() >= deadline {
            return Err(LegacyJournalObservationError("deadline elapsed"));
        }
        profile.validate_current().map_err(refuse)?;
        if profile.owner_identity() != self.profile_guard.owner_identity() {
            return Err(LegacyJournalObservationError("foreign profile acquisition"));
        }
        self.database_root
            .validate_profile_guard(profile)
            .map_err(refuse)?;
        self.validate_namespace().map_err(refuse)?;
        let _progress = Progress::install(&self.connection, deadline);
        let transaction = self.connection.unchecked_transaction().map_err(refuse)?;
        let result = (|| {
            // No SQL sorting is needed for this scalar bound before the existing exact-schema
            // validator reads its finite bounded image. Refuse oversized names/DDL first.
            let excessive: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE length(CAST(name AS BLOB))>256 OR length(CAST(sql AS BLOB))>16384)", [], |r| r.get(0)).map_err(refuse)?;
            let objects: u64 = self
                .connection
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(refuse)?;
            let temp: bool = self
                .connection
                .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_temp_schema)", [], |r| {
                    r.get(0)
                })
                .map_err(refuse)?;
            let databases: u64 = self
                .connection
                .query_row("SELECT count(*) FROM pragma_database_list", [], |r| {
                    r.get(0)
                })
                .map_err(refuse)?;
            if excessive || objects > 64 || temp || databases != 2 {
                return Err(LegacyJournalObservationError(
                    "unbounded, temporary or attached schema",
                ));
            }
            super::verify_connection_configuration(
                &self.connection,
                &self.database_path,
                JOURNAL_SCHEMA_VERSION,
            )
            .map_err(refuse)?;
            super::validate_existing_schema(
                &self.connection,
                &self.database_path,
                JOURNAL_SCHEMA_VERSION,
            )
            .map_err(refuse)?;
            let (main_bytes, sector) = native_facts(&self.connection).ok_or(
                LegacyJournalObservationError("unknown retained runtime/VFS"),
            )?;
            let read = |name| {
                self.connection
                    .pragma_query_value(None, name, |r| r.get::<_, u64>(0))
                    .map_err(refuse)
            };
            let page_size = read("page_size")?;
            let page_count = read("page_count")?;
            let current_max_pages = read("max_page_count")?;
            let cache_spill = read("cache_spill")?;
            let temp_store = read("temp_store")?;
            if temp_store > 2 {
                return Err(LegacyJournalObservationError(
                    "unknown temporary-store policy",
                ));
            }
            let selected_max_pages = select(page_size, page_count, current_max_pages, main_bytes)
                .ok_or(LegacyJournalObservationError(
                "unknown or exceeded journal cap",
            ))?;
            let (database_bound, rollback_bound, peak) = peak(page_size, selected_max_pages)
                .ok_or(LegacyJournalObservationError("journal estimate overflow"))?;
            let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
                &self.database_root,
                Path::new(JOURNAL_DATABASE_FILE_NAME),
                &self.database_path,
            )
            .map_err(refuse)?;
            if opened.identity != self.database_identity
                || opened.file.metadata().map_err(refuse)?.len() != main_bytes
            {
                return Err(LegacyJournalObservationError("journal descriptor changed"));
            }
            let facts =
                crate::durable_capacity::descriptor_capacity_facts(&opened.file).map_err(refuse)?;
            let (lock_bytes, lock_facts) = self
                .profile_guard
                .retained_lock_allocation_facts()
                .map_err(refuse)?;
            if !valid_unit(facts.allocation_unit)
                || !valid_unit(lock_facts.allocation_unit)
                || lock_bytes > 4096
            {
                return Err(LegacyJournalObservationError(
                    "unknown unit or unbounded profile control",
                ));
            }
            let directory = self.database_root.try_clone_descriptor().map_err(refuse)?;
            let directory_facts =
                crate::durable_capacity::descriptor_capacity_facts(&directory).map_err(refuse)?;
            if !valid_unit(directory_facts.allocation_unit) {
                return Err(LegacyJournalObservationError("unknown directory unit"));
            }
            let (rollback_bytes, rollback_identity) = self.rollback_facts()?;
            if rollback_bytes > rollback_bound {
                return Err(LegacyJournalObservationError(
                    "existing rollback exceeds proposed allowance",
                ));
            }
            self.validate_namespace_for_precommit().map_err(refuse)?;
            if Instant::now() >= deadline {
                return Err(LegacyJournalObservationError("deadline elapsed"));
            }
            Ok(LegacyJournalObservation {
                database_identity: self.database_identity.clone(),
                profile_identity: profile.profile_root_identity().into(),
                lock_identity: profile.profile_lock_identity().into(),
                owner_identity: profile.owner_identity().into(),
                volume: facts.identity,
                unit: facts.allocation_unit,
                directory_identity: self.database_root.identity().into(),
                sidecar_volume: directory_facts.identity,
                sidecar_unit: directory_facts.allocation_unit,
                main_bytes,
                rollback_bytes,
                rollback_identity,
                page_size,
                page_count,
                current_max_pages,
                selected_max_pages,
                sector,
                cache_spill,
                temp_store,
                lock_bytes,
                lock_volume: lock_facts.identity,
                lock_unit: lock_facts.allocation_unit,
                database_bound,
                rollback_bound,
                peak,
            })
        })();
        // An elapsed query deadline must not interrupt confirmation of the read rollback.
        drop(_progress);
        transaction.rollback().map_err(refuse)?;
        self.validate_namespace().map_err(refuse)?;
        profile.validate_current().map_err(refuse)?;
        let result = result?;
        if self.rollback_facts()? != (result.rollback_bytes, result.rollback_identity.clone())
            || Instant::now() >= deadline
        {
            return Err(LegacyJournalObservationError(
                "sidecar changed or deadline elapsed",
            ));
        }
        Ok(result)
    }
    pub(crate) fn rollback_facts(&self) -> Result<(u64, Option<String>)> {
        let name = format!("{JOURNAL_DATABASE_FILE_NAME}-journal");
        match self
            .database_root
            .open_observation_file_nofollow(Path::new(&name))
        {
            Ok(file) => {
                let metadata = file.metadata().map_err(refuse)?;
                if !metadata.is_file() {
                    return Err(LegacyJournalObservationError("rollback is not regular"));
                }
                let identity =
                    crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file)
                        .ok_or(LegacyJournalObservationError("rollback identity unknown"))?;
                let directory = self.database_root.try_clone_descriptor().map_err(refuse)?;
                let directory_facts =
                    crate::durable_capacity::descriptor_capacity_facts(&directory)
                        .map_err(refuse)?;
                let facts =
                    crate::durable_capacity::descriptor_capacity_facts(&file).map_err(refuse)?;
                // The initial logical model does not qualify a separately mounted or unknown
                // retained rollback object. Refuse, rather than charge it on the main volume.
                if facts.identity != directory_facts.identity
                    || facts.allocation_unit != directory_facts.allocation_unit
                {
                    return Err(LegacyJournalObservationError(
                        "rollback is on a foreign directory volume",
                    ));
                }
                Ok((metadata.len(), Some(identity)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, None)),
            Err(e) => Err(refuse(e)),
        }
    }
}
fn valid_unit(unit: u64) -> bool {
    unit != 0 && unit.is_power_of_two()
}
fn select(p: u64, count: u64, existing: u64, bytes: u64) -> Option<u64> {
    if !(512..=65536).contains(&p) || !p.is_power_of_two() || count == 0 || existing == 0 {
        return None;
    }
    let m = existing.min(CAP / p);
    (count <= m && bytes >= count.checked_mul(p)? && bytes <= m.checked_mul(p)?).then_some(m)
}
fn peak(p: u64, m: u64) -> Option<(u64, u64, u64)> {
    if p == 0 || m == 0 {
        return None;
    }
    let db = p.checked_mul(m)?;
    let rollback = m
        .checked_add(1)?
        .checked_mul(65536)?
        .checked_add(m.checked_mul(p.checked_add(8)?)?)?;
    Some((db, rollback, db.checked_add(rollback)?))
}
struct Progress<'a>(&'a Connection);
impl<'a> Progress<'a> {
    fn install(connection: &'a Connection, deadline: Instant) -> Self {
        connection.progress_handler(128, Some(move || Instant::now() >= deadline));
        Self(connection)
    }
}
impl Drop for Progress<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
fn native_facts(connection: &Connection) -> Option<(u64, u64)> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = connection;
        None
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if rusqlite::version_number() != 3_050_002 {
            return None;
        }
        let mut vfs: *mut ffi::sqlite3_vfs = std::ptr::null_mut();
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        // SAFETY: pointers borrowed from the exclusively held non-Sync connection only within
        // this call. Validate actual VFS identity before invoking its read-only native callbacks.
        unsafe {
            if ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_VFS_POINTER,
                (&mut vfs as *mut *mut ffi::sqlite3_vfs).cast(),
            ) != ffi::SQLITE_OK
            {
                return None;
            }
            #[cfg(target_os = "linux")]
            let expected = ffi::sqlite3_vfs_find(c"yield-linux-retained-root-v1".as_ptr());
            #[cfg(target_os = "macos")]
            let expected = ffi::sqlite3_vfs_find(c"unix".as_ptr());
            if expected.is_null() || vfs != expected {
                return None;
            }
            if ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast(),
            ) != ffi::SQLITE_OK
                || file.is_null()
            {
                return None;
            }
            let methods = (*file).pMethods.as_ref()?;
            let mut bytes = -1;
            if (methods.xFileSize?)(file, &mut bytes) != ffi::SQLITE_OK {
                return None;
            }
            let sector = u64::try_from((methods.xSectorSize?)(file)).ok()?;
            ((512..=65536).contains(&sector) && sector.is_power_of_two())
                .then_some((u64::try_from(bytes).ok()?, sector))
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
mod tests;
