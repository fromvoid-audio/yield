//! Exact current global schema and retained empty-writer allocation observations.
use super::{LIBRARY_DB_FILE_NAME, connection::LibraryDatabase};
use crate::lifecycle_allocation::{
    self, LifecycleAllocationError, LifecycleAllocationRole, LifecycleDatabaseAllocation,
};
use crate::sample_sources::db::schema::{bounded_runtime_image, bounded_schema_image};
use std::{path::Path, sync::OnceLock, time::Instant};

type Image = (Vec<(String, String, String, Option<String>)>, Vec<String>);
fn known_schema(database: &LibraryDatabase) -> Result<(), LifecycleAllocationError> {
    use LifecycleAllocationError::Unknown;
    if crate::sqlite_ext::extension_load_attempted() {
        return Err(Unknown);
    }
    let connection = &database.connection;
    let attached: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name NOT IN ('main','temp'))",
            [],
            |r| r.get(0),
        )
        .map_err(|_| Unknown)?;
    let temp: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM temp.sqlite_schema LIMIT 1)",
            [],
            |r| r.get(0),
        )
        .map_err(|_| Unknown)?;
    if attached || temp {
        return Err(Unknown);
    }
    static EXPECTED: OnceLock<Option<Image>> = OnceLock::new();
    let expected = EXPECTED
        .get_or_init(|| {
            let reference = LibraryDatabase::compiled_schema_reference().ok()?;
            Some((
                bounded_schema_image(&reference.connection).ok()?,
                bounded_runtime_image(&reference.connection).ok()?,
            ))
        })
        .as_ref()
        .ok_or(Unknown)?;
    let actual = (
        bounded_schema_image(connection).map_err(|_| Unknown)?,
        bounded_runtime_image(connection).map_err(|_| Unknown)?,
    );
    if &actual != expected {
        return Err(Unknown);
    }
    Ok(())
}
fn counts(database: &LibraryDatabase) -> Result<(u64, u64, u64), LifecycleAllocationError> {
    use LifecycleAllocationError::Unknown;
    let read = |sql| {
        database
            .connection
            .query_row(sql, [], |r| r.get::<_, u64>(0))
            .map_err(|_| Unknown)
    };
    let references =
        read("SELECT count(*) FROM (SELECT 1 FROM source_lifecycle_references LIMIT 8193)")?;
    let receipts = read(
        "SELECT count(*) FROM (SELECT 1 FROM source_lifecycle_reference_receipts LIMIT 8193)",
    )?;
    if references > 8192 || receipts > 8192 {
        return Err(Unknown);
    }
    let active = read(
        "SELECT count(*) FROM (SELECT 1 FROM source_lifecycle_references WHERE state IN ('Enrolled','Draining') LIMIT 4097)",
    )?;
    if active > 4096 {
        return Err(Unknown);
    }
    Ok((references, receipts, active))
}
impl LibraryDatabase {
    pub(super) fn capture_lifecycle_allocation(
        &self,
        deadline: Instant,
    ) -> Result<LifecycleDatabaseAllocation, LifecycleAllocationError> {
        // Existing worker BEGIN IMMEDIATE owns writer exclusion. Never start a second owner
        // or adopt an arbitrary autocommit connection as an actual retained reservation.
        if self.connection.is_autocommit() {
            return Err(LifecycleAllocationError::Unknown);
        }
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LifecycleAllocationError::Unknown)?;
        lifecycle_allocation::bounded_read(&self.connection, deadline, || {
            known_schema(self)?;
            let before_counts = counts(self)?;
            let root = self
                .preparation_root()
                .ok_or(LifecycleAllocationError::Unknown)?;
            let facts = lifecycle_allocation::capture(
                &self.connection,
                root,
                Path::new(LIBRARY_DB_FILE_NAME),
                LifecycleAllocationRole::GlobalReferences,
                deadline,
            )?;
            known_schema(self)?;
            if counts(self)? != before_counts
                || lifecycle_allocation::capture(
                    &self.connection,
                    root,
                    Path::new(LIBRARY_DB_FILE_NAME),
                    LifecycleAllocationRole::GlobalReferences,
                    deadline,
                )? != facts
            {
                return Err(LifecycleAllocationError::Unknown);
            }
            Ok(facts)
        })
    }
}

#[cfg(test)]
mod tests;
