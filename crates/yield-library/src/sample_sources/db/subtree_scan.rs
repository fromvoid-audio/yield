//! Bounded source-owned retained facts for one complete targeted subtree observation.
use super::{
    SourceDatabase, SourceDbError, SourceDirectoryEntry, SourceTraversalPolicy, SourceWriteBatch,
    read_bounded_numeric_metadata,
};
use crate::sample_sources::reconciliation::RootRelativePath;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
mod query;
#[cfg(test)]
mod tests;
mod transaction;

/// Fixed retained-row bound across all scopes and tables (including missing audio rows).
pub const SUBTREE_SCAN_MAX_RETAINED: usize = 128;
/// Private snapshot binding; this observation cannot be created or retargeted by a caller.
pub struct SubtreeScanDatabaseFacts {
    binding: Arc<()>,
    revision: u64,
    index_revision: u64,
    generation: i64,
    expected_count: i64,
    published_revision: i64,
    scopes: Vec<RootRelativePath>,
    audio: Vec<PathBuf>,
    index: Vec<PathBuf>,
    directories: Vec<SourceDirectoryEntry>,
    policy: SourceTraversalPolicy,
}
impl SubtreeScanDatabaseFacts {
    /// Generic revision captured in the same read snapshot.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Index revision captured in the same read snapshot.
    pub fn index_revision(&self) -> u64 {
        self.index_revision
    }
    /// Retained traversal policy.
    pub fn policy(&self) -> SourceTraversalPolicy {
        self.policy
    }
    /// At most 128 retained audio paths; no curation or content is hydrated.
    pub fn audio_paths(&self) -> &[PathBuf] {
        &self.audio
    }
    /// Bounded lossless retained index locators.
    pub fn index_paths(&self) -> &[PathBuf] {
        &self.index
    }
    /// Bounded previously complete directory truth inside the selected scopes.
    pub fn directories(&self) -> &[SourceDirectoryEntry] {
        &self.directories
    }
}

pub(super) fn active(
    connection: &rusqlite::Connection,
    revision: u64,
) -> Result<(i64, i64, i64), SourceDbError> {
    let mut stmt = connection.prepare("SELECT generation, expected_entry_count, staged_entry_count, complete, published_source_revision FROM source_directory_generations WHERE status = 'active' LIMIT 2")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
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
        || *published as u64 > revision
    {
        return Err(SourceDbError::Unexpected);
    }
    if connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM source_directory_generations WHERE status = 'staging')",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0
    {
        return Err(SourceDbError::Unexpected);
    }
    Ok((*generation, *expected, *published))
}

pub(super) fn validate_scopes(scopes: &[RootRelativePath]) -> Result<(), SourceDbError> {
    if scopes.is_empty() || scopes.len() > 64 {
        return Err(SourceDbError::Unexpected);
    }
    let mut bytes = 0usize;
    let mut unique = BTreeSet::new();
    for s in scopes {
        bytes = bytes.saturating_add(s.as_path().as_os_str().as_encoded_bytes().len());
        if s.as_path().as_os_str().as_encoded_bytes().len() > 1024
            || s.as_path().components().count() > 32
            || bytes > 32768
            || !unique.insert(s.as_path())
        {
            return Err(SourceDbError::Unexpected);
        }
    }
    // Overlapping scopes would duplicate retained rows and make count/absence accounting ambiguous.
    if scopes.iter().enumerate().any(|(i, a)| {
        scopes
            .iter()
            .enumerate()
            .any(|(j, b)| i != j && a.as_path().starts_with(b.as_path()))
    }) {
        return Err(SourceDbError::Unexpected);
    }
    Ok(())
}
