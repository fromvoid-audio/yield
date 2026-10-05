//! Exact current global schema and retained empty-writer allocation observations.
use super::{LIBRARY_DB_FILE_NAME, connection::LibraryDatabase};
use crate::lifecycle_allocation::{
    self, LifecycleAllocationError, LifecycleAllocationRole, LifecycleDatabaseAllocation,
};
use crate::sample_sources::db::schema::{
    bounded_runtime_image, bounded_schema_image, validate_bounded_runtime, validate_bounded_schema,
};
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
    validate_bounded_schema(connection, &expected.0).map_err(|_| Unknown)?;
    validate_bounded_runtime(connection, &expected.1).map_err(|_| Unknown)?;
    Ok(())
}
mod row_shapes;
use row_shapes::counts;
impl LibraryDatabase {
    pub(super) fn capture_lifecycle_allocation(
        &self,
        deadline: Instant,
    ) -> Result<LifecycleDatabaseAllocation, LifecycleAllocationError> {
        self.capture_lifecycle_allocation_checked(deadline, false)
    }
    // Passive policy selection is never a reservation; exact facts MUST be revalidated
    // after the sole worker acquires BEGIN IMMEDIATE on this same original connection.
    pub(super) fn capture_lifecycle_policy_input(
        &self,
        deadline: Instant,
    ) -> Result<LifecycleDatabaseAllocation, LifecycleAllocationError> {
        self.capture_lifecycle_allocation_checked(deadline, true)
    }
    fn capture_lifecycle_allocation_checked(
        &self,
        deadline: Instant,
        autocommit: bool,
    ) -> Result<LifecycleDatabaseAllocation, LifecycleAllocationError> {
        if self.connection.is_autocommit() != autocommit {
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

pub(super) mod policy;

#[cfg(test)]
mod tests;
