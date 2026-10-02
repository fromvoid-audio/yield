//! Layout-specific retained allocation observations. These values never grant writes.
use super::*;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Weak;

/// Maximum encoded source-owned layout generation bytes.
pub const SOURCE_LAYOUT_GENERATION_MAX_BYTES: u64 = 512 * 1024;
pub(crate) const LAYOUT_GENERATION_KEY: &str = "source_starmap_generation_v1";
const MAX_DATABASE_BYTES: u64 = 64 * 1024 * 1024;

/// Private owner-issued storage facts for one bounded derived-layout generation.
/// Existing bytes are measured, not adopted as a valid canonical generation.
#[derive(Clone, Debug)]
pub struct SourceLayoutMetadataBudget {
    revision: SourceRevision,
    owner: Weak<()>,
    pub(super) deadline: Instant,
    page_size: u64,
    page_count: u64,
    database_bytes: u64,
    max_pages: u64,
    wal_bytes: u64,
    shm_bytes: u64,
    old_bytes: u64,
    old_digest: Option<[u8; 32]>,
    required_bytes: u64,
}
impl SourceLayoutMetadataBudget {
    /// Exact retained source scope whose storage was observed.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Observed SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Observed logical database pages, distinct from physical file length.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Main database length through the retained SQLite VFS descriptor.
    pub fn resident_database_bytes(&self) -> u64 {
        self.database_bytes
    }
    /// Maximum pages that the entered policy must enforce.
    pub fn maximum_pages(&self) -> u64 {
        self.max_pages
    }
    /// Resident retained WAL allocation included in the plan.
    pub fn resident_wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Resident WAL-index bytes through the retained source root.
    pub fn resident_shm_bytes(&self) -> u64 {
        self.shm_bytes
    }
    /// Size of bounded existing metadata, without validating its semantic generation.
    pub fn existing_generation_bytes(&self) -> u64 {
        self.old_bytes
    }
    /// Digest of observed old bytes; not a publication receipt or adopted generation.
    pub fn existing_generation_digest(&self) -> Option<[u8; 32]> {
        self.old_digest
    }
    /// Maximum canonical generation size for the future validated publication path.
    pub fn maximum_generation_bytes(&self) -> u64 {
        SOURCE_LAYOUT_GENERATION_MAX_BYTES
    }
    /// Conservative disk peak, including complete capped DB/index and recovery WAL work.
    pub fn required_metadata_bytes(&self) -> u64 {
        self.required_bytes
    }
    /// Bounded old/new encoding custody, separate from the disk allocation observation.
    pub fn maximum_generation_custody_bytes(&self) -> u64 {
        self.old_bytes + SOURCE_LAYOUT_GENERATION_MAX_BYTES
    }
    /// Original cooperative observation deadline, never extended by validation.
    pub fn observation_deadline(&self) -> Instant {
        self.deadline
    }
}
fn unknown() -> SourceWriterError {
    SourceWriterError::MetadataBoundNotDefined
}

impl SourceWriter {
    /// Measure this retained connection under strict read-only ownership and known schema.
    /// No generation validation/publication, lease renewal, capacity claim or intent is issued.
    pub fn capture_layout_metadata_budget(
        &self,
        expected: &SourceRevision,
    ) -> Result<SourceLayoutMetadataBudget, SourceWriterError> {
        if !self.database.connection.is_autocommit()
            || self.database.tag_metadata_policy_state.get()
                != crate::sample_sources::db::TAG_METADATA_POLICY_IDLE
        {
            return Err(unknown());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let _extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or_else(unknown)?;
        let before = self.current_revision()?;
        if expected != &before {
            return Err(SourceWriterError::StaleFence);
        }
        let connection = &self.database.connection;
        crate::sample_sources::db::schema::require_known_metadata_schema(connection)
            .map_err(|_| unknown())?;
        let read = |name| {
            connection
                .pragma_query_value(None, name, |r| r.get::<_, u64>(0))
                .map_err(|_| unknown())
        };
        let page_size = read("page_size")?;
        let page_count = read("page_count")?;
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() || page_count == 0 {
            return Err(unknown());
        }
        let database_bytes =
            crate::sqlite_wal::retained_database_bytes(connection).map_err(|_| unknown())?;
        let existing_max = read("max_page_count")?;
        let locking: String = connection
            .pragma_query_value(None, "locking_mode", |r| r.get(0))
            .map_err(|_| unknown())?;
        if !locking.eq_ignore_ascii_case("normal") || existing_max == 0 {
            return Err(unknown());
        }
        let max_pages = existing_max.min(MAX_DATABASE_BYTES / page_size);
        if page_count > max_pages
            || database_bytes > page_size.checked_mul(max_pages).ok_or_else(unknown)?
        {
            return Err(unknown());
        }
        crate::sqlite_wal::enforce_wal_admission(connection).map_err(SourceDbError::from)?;
        let wal_bytes =
            crate::sqlite_wal::retained_wal_bytes(connection).map_err(SourceDbError::from)?;
        let binding = self
            .database
            .source_lease
            .as_ref()
            .ok_or_else(unknown)?
            .database_root_binding();
        let mut shm_name = OsString::from(self.database.db_path.file_name().ok_or_else(unknown)?);
        shm_name.push("-shm");
        let file = binding
            .open_file_read_nofollow(Path::new(&shm_name))
            .map_err(|_| unknown())?;
        let metadata = file.metadata().map_err(|_| unknown())?;
        let shm_bytes = metadata.len();
        if !metadata.is_file() || shm_bytes < 32768 || shm_bytes % 32768 != 0 {
            return Err(unknown());
        }
        let (old_bytes, old_digest) = generation_observation(connection)?;
        let required_bytes =
            checked_peak(page_size, max_pages, wal_bytes, shm_bytes).ok_or_else(unknown)?;
        if expected != &self.current_revision()? {
            return Err(SourceWriterError::StaleFence);
        }
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(SourceLayoutMetadataBudget {
            revision: before,
            owner: Arc::downgrade(&self.reference_liveness),
            deadline,
            page_size,
            page_count,
            database_bytes,
            max_pages,
            wal_bytes,
            shm_bytes,
            old_bytes,
            old_digest,
            required_bytes,
        })
    }

    /// Recheck all captured facts without extending the original observation deadline.
    pub fn validate_layout_metadata_budget(
        &self,
        budget: &SourceLayoutMetadataBudget,
    ) -> Result<(), SourceWriterError> {
        if Instant::now() >= budget.deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let owner = budget
            .owner
            .upgrade()
            .ok_or(SourceWriterError::StaleFence)?;
        if !Arc::ptr_eq(&owner, &self.reference_liveness) {
            return Err(SourceWriterError::StaleFence);
        }
        let current = self.capture_layout_metadata_budget(&budget.revision)?;
        if (
            current.page_size,
            current.page_count,
            current.database_bytes,
            current.max_pages,
            current.wal_bytes,
            current.shm_bytes,
            current.old_bytes,
            current.old_digest,
            current.required_bytes,
        ) != (
            budget.page_size,
            budget.page_count,
            budget.database_bytes,
            budget.max_pages,
            budget.wal_bytes,
            budget.shm_bytes,
            budget.old_bytes,
            budget.old_digest,
            budget.required_bytes,
        ) {
            return Err(unknown());
        }
        if Instant::now() >= budget.deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(())
    }
}

pub(super) fn generation_observation(
    connection: &rusqlite::Connection,
) -> Result<(u64, Option<[u8; 32]>), SourceWriterError> {
    let mut statement = connection
        .prepare(
            "SELECT typeof(value), length(CAST(value AS BLOB)), CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND 524288 THEN value ELSE NULL END FROM metadata WHERE key=?1",
        )
        .map_err(|_| unknown())?;
    let mut rows = statement
        .query([LAYOUT_GENERATION_KEY])
        .map_err(|_| unknown())?;
    let Some(row) = rows.next().map_err(|_| unknown())? else {
        return Ok((0, None));
    };
    let kind: String = row.get(0).map_err(|_| unknown())?;
    let bytes: u64 = row.get(1).map_err(|_| unknown())?;
    if kind != "text" || bytes == 0 || bytes > SOURCE_LAYOUT_GENERATION_MAX_BYTES {
        return Err(unknown());
    }
    let raw = row
        .get_ref(2)
        .map_err(|_| unknown())?
        .as_str()
        .map_err(|_| unknown())?;
    if raw.len() as u64 != bytes || raw.contains('\0') {
        return Err(unknown());
    }
    Ok((bytes, Some(*blake3::hash(raw.as_bytes()).as_bytes())))
}

fn checked_peak(page_size: u64, max_pages: u64, wal: u64, shm: u64) -> Option<u64> {
    let database = page_size.checked_mul(max_pages)?;
    if database > MAX_DATABASE_BYTES {
        return None;
    }
    // Complete page cap covers old/new metadata, its index and every other database page.
    // With spill/checkpoint disabled, charge another complete DB worth of WAL frames plus
    // resident retained WAL and both resident/growth WAL-index storage for recovery custody.
    let new_wal = max_pages
        .checked_mul(page_size.checked_add(24)?)?
        .checked_add(32)?;
    let new_shm = max_pages.checked_mul(16)?.checked_add(32768)?;
    database
        .checked_add(wal)?
        .checked_add(new_wal)?
        .checked_add(shm)?
        .checked_add(new_shm)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_metadata_peak_checks_all_terms_and_overflow() {
        let p = 4096;
        let m = MAX_DATABASE_BYTES / p;
        assert_eq!(
            checked_peak(p, m, 101, 32768),
            Some(MAX_DATABASE_BYTES + 101 + m * (p + 24) + 32 + 32768 + m * 16 + 32768)
        );
        assert_eq!(checked_peak(p, m + 1, 0, 0), None);
        assert_eq!(checked_peak(u64::MAX, 2, 0, 0), None);
        assert_eq!(checked_peak(p, m, u64::MAX, 0), None);
        assert_eq!(checked_peak(p, m, 0, u64::MAX), None);
    }
}
