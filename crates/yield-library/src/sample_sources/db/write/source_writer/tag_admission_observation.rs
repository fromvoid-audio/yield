//! Retained source provenance for metadata-only journal preparation; never admission authority.
use super::*;
use crate::sample_sources::db::{BoundDatabaseRoot, source_lease::SourceLease};
use std::fs::File;
use std::sync::Weak;

/// Exact source scope, measured storage budget and matching retained metadata descriptor.
/// Private fields/absence of Deserialize prevent manufacturing this from historical records.
/// This observation does not issue capacity, journal identity, source writes or completion.
pub struct SourceTagAdmissionObservation {
    selection: SourceTagAssignmentSelection,
    budget: SourceTagMetadataBudget,
    directory_generation: u64,
    descriptor: File,
    _root: Arc<BoundDatabaseRoot>,
    owner: Weak<()>,
    lease: Weak<SourceLease>,
}
impl SourceTagAdmissionObservation {
    /// Full bounded assignment scope, not a file-operation locator/capability.
    pub fn selection(&self) -> &SourceTagAssignmentSelection {
        &self.selection
    }
    /// Actual source-owner physical observations; entered enforcement/claims remain required.
    pub fn budget(&self) -> &SourceTagMetadataBudget {
        &self.budget
    }
    /// Complete active directory generation frozen in the selection's SQLite read snapshot.
    pub fn directory_generation(&self) -> u64 {
        self.directory_generation
    }

    /// Retained source liveness/physical authority only, never a claim or final mutation check.
    /// The receiving source actor must still revalidate the entire budget/selection at effect.
    pub(crate) fn validate_live_origin(&self) -> Result<(), SourceWriterError> {
        let _owner = self.owner.upgrade().ok_or(SourceWriterError::StaleFence)?;
        let lease = self.lease.upgrade().ok_or(SourceWriterError::StaleFence)?;
        lease
            .validate_current_owner()
            .map_err(SourceDbError::from)?;
        if crate::filesystem_identity::stable_filesystem_identity_from_open_file(&self.descriptor)
            .as_deref()
            != Some(self.selection.revision().database_identity())
            || self
                .descriptor
                .metadata()
                .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?
                .len()
                != self.budget.resident_database_bytes()
        {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(())
    }

    /// Borrow only inside the private capacity planner, after current provenance validation.
    /// Native volume facts derive from this actual matching descriptor, not caller scalars.
    pub(crate) fn capacity_descriptor(&self) -> Result<&File, SourceWriterError> {
        self.validate_live_origin()?;
        Ok(&self.descriptor)
    }
}

impl SourceWriter {
    /// Revalidate every measured fact and the complete selection on its original owner.
    /// A successful check is still an observation, never journal admission or write authority.
    pub fn validate_tag_admission_observation(
        &self,
        observation: &SourceTagAdmissionObservation,
    ) -> Result<(), SourceWriterError> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(SourceWriterError::MetadataBoundNotDefined)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        observation.validate_live_origin()?;
        self.validate_tag_metadata_budget(&observation.budget)?;
        let selected = &observation.selection;
        let current = self.capture_tag_admission_observation(
            selected.revision(),
            selected.old_identity(),
            selected.mutation().clone(),
        )?;
        if current.selection != *selected
            || current.directory_generation != observation.directory_generation
        {
            return Err(SourceWriterError::StaleFence);
        }
        self.validate_tag_metadata_budget(&observation.budget)?;
        observation.validate_live_origin()?;
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(())
    }

    /// Capture source-only journal preparation input without creating an intent or write grant.
    pub fn capture_tag_admission_observation(
        &self,
        expected: &SourceRevision,
        old_label: &str,
        mutation: SourceTagAssignmentMutation,
    ) -> Result<SourceTagAdmissionObservation, SourceWriterError> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(SourceWriterError::MetadataBoundNotDefined)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let budget = self.capture_tag_metadata_budget(expected)?;
        let selection;
        let directory_generation;
        {
            let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
            let before = self.current_revision()?;
            if &before != expected {
                return Err(SourceWriterError::StaleFence);
            }
            let tx = self
                .database
                .connection
                .unchecked_transaction()
                .map_err(map_sql_error)?;
            if self.database.get_revision()? != expected.revision() {
                return Err(SourceWriterError::StaleFence);
            }
            selection = tag_assignment_selection::capture(&tx, before, old_label, mutation)?;
            directory_generation = active_generation(&tx, expected.revision())?;
            tx.commit().map_err(map_sql_error)?;
            if &self.current_revision()? != expected {
                return Err(SourceWriterError::StaleFence);
            }
        }
        self.validate_tag_metadata_budget(&budget)?;
        let lease = self
            .database
            .source_lease
            .as_ref()
            .ok_or(SourceWriterError::StaleFence)?;
        let root = lease.database_root_binding().clone();
        let name = self
            .database
            .db_path
            .file_name()
            .ok_or(SourceWriterError::StaleFence)?;
        let descriptor = root
            .open_file_read_nofollow(std::path::Path::new(name))
            .map_err(|_| SourceWriterError::MetadataBoundNotDefined)?;
        let observation = SourceTagAdmissionObservation {
            selection,
            budget,
            directory_generation,
            descriptor,
            _root: root,
            owner: Arc::downgrade(&self.reference_liveness),
            lease: Arc::downgrade(lease),
        };
        let _ = observation.capacity_descriptor()?;
        if &self.current_revision()? != expected {
            return Err(SourceWriterError::StaleFence);
        }
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        Ok(observation)
    }
}

fn active_generation(
    connection: &rusqlite::Connection,
    revision: u64,
) -> Result<u64, SourceWriterError> {
    let mut stmt = connection.prepare("SELECT generation,expected_entry_count,staged_entry_count,complete,published_source_revision FROM source_directory_generations WHERE status='active' LIMIT 2").map_err(map_sql_error)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    let [(generation, expected, staged, complete, published)] = rows.as_slice() else {
        return Err(SourceWriterError::StaleFence);
    };
    if *generation <= 0
        || *expected < 0
        || expected != staged
        || *complete != 1
        || u64::try_from(*published).ok() != Some(revision)
    {
        return Err(SourceWriterError::StaleFence);
    }
    let actual: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_directory_entries WHERE generation=?1",
            [generation],
            |r| r.get(0),
        )
        .map_err(map_sql_error)?;
    if actual != *expected {
        return Err(SourceWriterError::StaleFence);
    }
    Ok(*generation as u64)
}
