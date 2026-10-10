use super::*;
use std::collections::BTreeMap;

impl SourceWriteBatch<'_> {
    /// Replace only the directory facts covered by a complete held subtree traversal, preserving
    /// all other active facts. The private read observation binds scope/generation/revision to
    /// this owner. This stages publication for the *same* next generic revision as the manifest
    /// and watcher checkpoint; it never commits or advances a full-source audit marker.
    /// Callers must finish/revalidate the complete bounded traversal before opening this batch.
    pub fn apply_prepared_subtree_directory_truth(
        &mut self,
        facts: &SubtreeScanDatabaseFacts,
        entries: &[SourceDirectoryEntry],
    ) -> Result<(), SourceDbError> {
        if self.subtree_directory_revision.is_some()
            || !Arc::ptr_eq(&facts.binding, &self.owner.index_page_binding)
            || read_bounded_numeric_metadata(&self.tx, "revision")? != facts.revision
            || self.get_source_index_revision()? != facts.index_revision
            || active(&self.tx, facts.revision)?
                != (
                    facts.generation,
                    facts.expected_count,
                    facts.published_revision,
                )
            || entries.len() > 64
        {
            return Err(SourceDbError::Unexpected);
        }
        let mut paths = BTreeSet::new();
        let mut identities = BTreeSet::new();
        let mut encoded = Vec::with_capacity(entries.len());
        let mut bytes = 0usize;
        for entry in entries {
            bytes = bytes
                .saturating_add(entry.relative_path.as_os_str().as_encoded_bytes().len())
                .saturating_add(entry.directory_identity.len());
            if entry.relative_path.as_os_str().as_encoded_bytes().len() > 1024
                || entry.relative_path.components().count() > 32
                || bytes > 32768
                || !facts
                    .scopes
                    .iter()
                    .any(|s| entry.relative_path.starts_with(s.as_path()))
                || !paths.insert(&entry.relative_path)
                || !identities.insert(&entry.directory_identity)
            {
                return Err(SourceDbError::Unexpected);
            }
            super::super::directories::validate_directory_identity(&entry.directory_identity)?;
            let (key, encoding) =
                super::super::directories::encode_directory_path(&entry.relative_path)?;
            encoded.push((key, encoding, &entry.directory_identity));
        }
        let count = facts
            .expected_count
            .checked_sub(facts.directories.len() as i64)
            .filter(|c| *c >= 0)
            .and_then(|c| c.checked_add(entries.len() as i64))
            .ok_or(SourceDbError::Unexpected)?;
        let published = facts
            .revision
            .checked_add(1)
            .and_then(|r| i64::try_from(r).ok())
            .ok_or(SourceDbError::Unexpected)?;
        let savepoint = self.tx.savepoint()?;
        for previous in &facts.directories {
            let (key, encoding) =
                super::super::directories::encode_directory_path(&previous.relative_path)?;
            if savepoint.execute("DELETE FROM source_directory_entries WHERE generation=?1 AND path=?2 COLLATE BINARY AND path_encoding=?3 AND directory_identity=?4",rusqlite::params![facts.generation,key,encoding,previous.directory_identity])?!=1 { return Err(SourceDbError::Unexpected); }
        }
        for (key, encoding, identity) in encoded {
            // A physical identity already owned by another retained directory cannot be aliased.
            if savepoint.query_row("SELECT EXISTS(SELECT 1 FROM source_directory_entries WHERE generation=?1 AND directory_identity=?2 LIMIT 1)",rusqlite::params![facts.generation,identity],|r|r.get::<_,i64>(0))?!=0 { return Err(SourceDbError::Unexpected); }
            savepoint.execute("INSERT INTO source_directory_entries(generation,path,path_encoding,directory_identity) VALUES(?1,?2,?3,?4)",rusqlite::params![facts.generation,key,encoding,identity])?;
        }
        if savepoint.execute("UPDATE source_directory_generations SET expected_entry_count=?1, staged_entry_count=?1, published_source_revision=?2 WHERE generation=?3 AND status='active' AND complete=1 AND published_source_revision=?4",rusqlite::params![count,published,facts.generation,facts.published_revision])?!=1 { return Err(SourceDbError::Unexpected); }
        savepoint.commit()?;
        self.subtree_directory_revision = Some(published as u64);
        let mut changes = facts
            .directories
            .iter()
            .map(|d| (d.relative_path.clone(), None))
            .collect::<BTreeMap<_, _>>();
        for entry in entries {
            changes.insert(entry.relative_path.clone(), Some(entry.clone()));
        }
        self.subtree_directory_changes =
            Some((facts.generation as u64, changes.into_iter().collect()));
        Ok(())
    }
}
