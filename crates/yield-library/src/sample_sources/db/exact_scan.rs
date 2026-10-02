//! Scalar/existence-only input for bounded exact scanner preparation.
use super::{
    META_SOURCE_INDEX_REVISION, META_SOURCE_TRAVERSAL_POLICY, SourceDatabase, SourceDbError,
    read_bounded_numeric_metadata,
};
use crate::sample_sources::{SourceTraversalPolicy, reconciliation::RootRelativePath};
use std::path::PathBuf;

/// Bounded owner database observations; no content, curation list or physical-effect authority.
pub struct ExactScanDatabaseFacts {
    /// Index revision from the same source read snapshot.
    pub index_revision: u64,
    /// Fixed enum policy; unknown/oversized stored state cannot grant more traversal.
    pub policy: SourceTraversalPolicy,
    /// At most 64 exact Unicode rows already tracked by the audio table.
    pub tracked_paths: Vec<PathBuf>,
}
impl SourceDatabase {
    /// Read finite scalar/existence facts without materializing audio metadata or tag lists.
    pub fn exact_scan_database_facts_at_revision(
        &self,
        paths: &[RootRelativePath],
        expected_revision: u64,
    ) -> Result<ExactScanDatabaseFacts, SourceDbError> {
        if paths.len() > 64 {
            return Err(SourceDbError::Unexpected);
        }
        let transaction = self.connection.unchecked_transaction()?;
        if read_bounded_numeric_metadata(&transaction, "revision")? != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        let index_revision =
            read_bounded_numeric_metadata(&transaction, META_SOURCE_INDEX_REVISION)?;
        let policy = read_policy(&transaction)?;
        let mut tracked_paths = Vec::with_capacity(paths.len());
        let mut statement = transaction
            .prepare("SELECT 1 FROM wav_files WHERE path = ?1 COLLATE BINARY LIMIT 2")?;
        let mut bytes = 0usize;
        for path in paths {
            bytes = bytes.saturating_add(path.as_path().as_os_str().as_encoded_bytes().len());
            if path.as_path().as_os_str().as_encoded_bytes().len() > 1024
                || bytes > 32768
                || path.as_path().components().count() > 32
            {
                return Err(SourceDbError::Unexpected);
            }
            if path.as_path().to_str().is_none() {
                continue;
            }
            let key = super::normalize_relative_path(path.as_path())?;
            let rows = statement
                .query_map([key], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if rows.len() > 1 {
                return Err(SourceDbError::Unexpected);
            }
            if !rows.is_empty() {
                tracked_paths.push(path.as_path().to_path_buf());
            }
        }
        Ok(ExactScanDatabaseFacts {
            index_revision,
            policy,
            tracked_paths,
        })
    }
}

pub(super) fn read_policy(
    transaction: &rusqlite::Connection,
) -> Result<SourceTraversalPolicy, SourceDbError> {
    use super::replay_authority::{MetadataValue, read_metadata};
    let policy = match read_metadata(transaction, META_SOURCE_TRAVERSAL_POLICY)? {
        MetadataValue::Absent => SourceTraversalPolicy::default(),
        MetadataValue::Invalid => return Err(SourceDbError::Unexpected),
        MetadataValue::Value(value)
            if [
                SourceTraversalPolicy::include_hidden_directories().as_str(),
                SourceTraversalPolicy::exclude_hidden_directories().as_str(),
            ]
            .contains(&value.as_str()) =>
        {
            SourceTraversalPolicy::from_stored(&value)
        }
        MetadataValue::Value(_) => return Err(SourceDbError::Unexpected),
    };
    Ok(policy)
}
