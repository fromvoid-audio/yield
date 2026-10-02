//! Bounded retained directory facts for exact replay namespace checks.
use super::{SourceDatabase, SourceDbError, SourceDirectoryEntry, read_bounded_numeric_metadata};
use crate::sample_sources::reconciliation::RootRelativePath;
use std::collections::BTreeSet;

impl SourceDatabase {
    /// Read at most 64 known non-root parents at this source revision, in one read snapshot.
    /// The caller must compare these retained facts with held no-follow directories. They are
    /// observations, not filesystem-effect authority; unknown/new parents require an audit.
    pub fn exact_scan_parent_directories_at_revision(
        &self,
        paths: &[RootRelativePath],
        expected_revision: u64,
    ) -> Result<Vec<SourceDirectoryEntry>, SourceDbError> {
        if paths.is_empty() || paths.len() > 64 {
            return Err(SourceDbError::Unexpected);
        }
        let mut parents = BTreeSet::new();
        let mut bytes = 0usize;
        for path in paths {
            if path.as_path().as_os_str().as_encoded_bytes().len() > 1024
                || path.as_path().components().count() > 32
            {
                return Err(SourceDbError::Unexpected);
            }
            for parent in path
                .as_path()
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                if parents.insert(parent.to_path_buf()) {
                    bytes = bytes.saturating_add(parent.as_os_str().as_encoded_bytes().len());
                }
                if parents.len() > 64 || bytes > 32768 {
                    return Err(SourceDbError::Unexpected);
                }
            }
        }
        let transaction = self.connection.unchecked_transaction()?;
        if read_bounded_numeric_metadata(&transaction, "revision")? != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        if parents.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = transaction.prepare("SELECT generation, expected_entry_count, staged_entry_count, complete, published_source_revision FROM source_directory_generations WHERE status = 'active' LIMIT 2")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let [(generation, expected, staged, complete, published)] = rows.as_slice() else {
            return Err(SourceDbError::Unexpected);
        };
        if *generation <= 0
            || *expected < 0
            || expected != staged
            || *complete != 1
            || *published <= 0
            || *published as u64 > expected_revision
        {
            return Err(SourceDbError::Unexpected);
        }
        let mut entries = Vec::with_capacity(parents.len());
        let mut query = transaction.prepare("SELECT CASE WHEN typeof(directory_identity) = 'text' AND length(CAST(directory_identity AS BLOB)) BETWEEN 1 AND 1024 THEN directory_identity END FROM source_directory_entries WHERE generation = ?1 AND path = ?2 COLLATE BINARY AND path_encoding = ?3 LIMIT 2")?;
        for parent in parents {
            let (key, encoding) = super::directories::encode_directory_path(&parent)?;
            let rows = query
                .query_map(rusqlite::params![generation, key, encoding], |row| {
                    row.get::<_, Option<String>>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let [Some(identity)] = rows.as_slice() else {
                return Err(SourceDbError::Unexpected);
            };
            super::directories::validate_directory_identity(identity)?;
            entries.push(SourceDirectoryEntry {
                relative_path: parent,
                directory_identity: identity.clone(),
            });
        }
        Ok(entries)
    }
}
