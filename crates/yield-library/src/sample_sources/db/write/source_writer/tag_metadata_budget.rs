//! Owner-bound observations for the first capped tag metadata transaction.
//! An observation is not an ordinary capacity claim, source mutation grant or journal receipt.
use super::*;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Weak;

const MAX_DATABASE_BYTES: u64 = 64 * 1024 * 1024;

/// Captured physical/source scope and conservative allocation bound, never write authority.
#[derive(Clone, Debug)]
pub struct SourceTagMetadataBudget {
    revision: SourceRevision,
    owner: Weak<()>,
    page_size: u64,
    page_count: u64,
    database_bytes: u64,
    max_pages: u64,
    resident_wal_bytes: u64,
    resident_shm_bytes: u64,
    required_bytes: u64,
}
impl SourceTagMetadataBudget {
    /// Exact owner-issued source scope whose facts were measured.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Captured SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Current logical database page count, distinct from retained physical file length.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Actual main database length through SQLite's retained VFS descriptor.
    pub fn resident_database_bytes(&self) -> u64 {
        self.database_bytes
    }
    /// Maximum pages that the transaction must actually enforce.
    pub fn maximum_pages(&self) -> u64 {
        self.max_pages
    }
    /// Resident retained-WAL bytes that cannot be silently excluded from the plan.
    pub fn resident_wal_bytes(&self) -> u64 {
        self.resident_wal_bytes
    }
    /// Resident WAL-index bytes observed through the retained source database root.
    pub fn resident_shm_bytes(&self) -> u64 {
        self.resident_shm_bytes
    }
    /// Conservative DB/WAL/SHM peak; journal/profile claims remain separate requirements.
    pub fn required_metadata_bytes(&self) -> u64 {
        self.required_bytes
    }
}

fn unknown() -> SourceWriterError {
    SourceWriterError::MetadataBoundNotDefined
}

impl SourceWriter {
    /// Capture capped metadata storage facts on the retained owner. No mutation or claim issued.
    /// The eventual participant must enforce/revalidate the policy and present real admission.
    pub fn capture_tag_metadata_budget(
        &self,
        expected: &SourceRevision,
    ) -> Result<SourceTagMetadataBudget, SourceWriterError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let _extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or_else(unknown)?;
        let before = self.current_revision()?;
        if expected != &before {
            return Err(SourceWriterError::StaleFence);
        }
        crate::sample_sources::db::schema::require_known_metadata_schema(&self.database.connection)
            .map_err(|_| unknown())?;
        let connection = &self.database.connection;
        let page_size: u64 = connection
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .map_err(|_| unknown())?;
        let page_count: u64 = connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .map_err(|_| unknown())?;
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() || page_count == 0 {
            return Err(unknown());
        }
        let database_bytes =
            crate::sqlite_wal::retained_database_bytes(connection).map_err(|_| unknown())?;
        if database_bytes > MAX_DATABASE_BYTES {
            return Err(unknown());
        }
        let existing_max_pages: u64 = connection
            .pragma_query_value(None, "max_page_count", |r| r.get(0))
            .map_err(|_| unknown())?;
        let locking_mode: String = connection
            .pragma_query_value(None, "locking_mode", |r| r.get(0))
            .map_err(|_| unknown())?;
        if !locking_mode.eq_ignore_ascii_case("normal") || existing_max_pages == 0 {
            return Err(unknown());
        }
        let max_pages = existing_max_pages.min(MAX_DATABASE_BYTES / page_size);
        if database_bytes > max_pages.checked_mul(page_size).ok_or_else(unknown)? {
            return Err(unknown());
        }
        if page_count > max_pages {
            return Err(unknown());
        }
        crate::sqlite_wal::enforce_wal_admission(connection).map_err(SourceDbError::from)?;
        let resident_wal_bytes =
            crate::sqlite_wal::retained_wal_bytes(connection).map_err(SourceDbError::from)?;
        let binding = self
            .database
            .source_lease
            .as_ref()
            .ok_or_else(unknown)?
            .database_root_binding();
        let name = self.database.db_path.file_name().ok_or_else(unknown)?;
        let mut shm_name = OsString::from(name);
        shm_name.push("-shm");
        let resident_shm_bytes = match binding.open_file_read_nofollow(Path::new(&shm_name)) {
            Ok(file) => {
                let metadata = file.metadata().map_err(|_| unknown())?;
                if !metadata.is_file() {
                    return Err(unknown());
                }
                metadata.len()
            }
            Err(_) => return Err(unknown()),
        };
        // Ordinary SQLite WAL uses 32 KiB WAL-index regions. Missing/truncated or malformed
        // namespace observations are not proof of zero allocation.
        if resident_shm_bytes < 32768 || resident_shm_bytes % 32768 != 0 {
            return Err(unknown());
        }
        let required_bytes =
            checked_peak(page_size, max_pages, resident_wal_bytes, resident_shm_bytes)
                .ok_or_else(unknown)?;
        if expected != &self.current_revision()? {
            return Err(SourceWriterError::StaleFence);
        }
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(SourceTagMetadataBudget {
            revision: before,
            owner: Arc::downgrade(&self.reference_liveness),
            page_size,
            page_count,
            database_bytes,
            max_pages,
            resident_wal_bytes,
            resident_shm_bytes,
            required_bytes,
        })
    }

    /// Revalidate the captured owner and current facts before constructing a real admission plan.
    pub fn validate_tag_metadata_budget(
        &self,
        budget: &SourceTagMetadataBudget,
    ) -> Result<(), SourceWriterError> {
        let owner = budget
            .owner
            .upgrade()
            .ok_or(SourceWriterError::StaleFence)?;
        if !Arc::ptr_eq(&owner, &self.reference_liveness) {
            return Err(SourceWriterError::StaleFence);
        }
        let current = self.capture_tag_metadata_budget(&budget.revision)?;
        if (
            current.page_size,
            current.page_count,
            current.database_bytes,
            current.max_pages,
            current.resident_wal_bytes,
            current.resident_shm_bytes,
            current.required_bytes,
        ) != (
            budget.page_size,
            budget.page_count,
            budget.database_bytes,
            budget.max_pages,
            budget.resident_wal_bytes,
            budget.resident_shm_bytes,
            budget.required_bytes,
        ) {
            return Err(unknown());
        }
        Ok(())
    }
}

fn checked_peak(page_size: u64, max_pages: u64, wal: u64, shm: u64) -> Option<u64> {
    let database = page_size.checked_mul(max_pages)?;
    if database > MAX_DATABASE_BYTES {
        return None;
    }
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
    fn tag_metadata_budget_peak_checks_every_allocation_and_overflow() {
        let p = 4096_u64;
        let m = MAX_DATABASE_BYTES / p;
        let expected = MAX_DATABASE_BYTES + 101 + 32 + m * (p + 24) + 32768 + 32768 + 16 * m;
        assert_eq!(checked_peak(p, m, 101, 32768), Some(expected));
        assert_eq!(checked_peak(p, m + 1, 0, 0), None);
        assert_eq!(checked_peak(u64::MAX, 2, 0, 0), None);
        assert_eq!(checked_peak(p, m, u64::MAX, 0), None);
        assert_eq!(checked_peak(p, m, 0, u64::MAX), None);
    }
}
