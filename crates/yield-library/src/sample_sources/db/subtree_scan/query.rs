use super::*;
use crate::sample_sources::db::util::{
    exact_subtree_path_bounds, normalize_source_index_path, parse_canonical_directory_path_from_db,
    parse_relative_path_from_db,
};

impl SourceDatabase {
    /// Read only bounded retained locators/identities within non-overlapping source-relative
    /// subtrees. Every table uses its exact BINARY path range and LIMIT lookahead. Malformed,
    /// oversized or incomplete state fails rather than granting an absence interpretation.
    pub fn subtree_scan_database_facts_at_revision(
        &self,
        scopes: &[RootRelativePath],
        revision: u64,
    ) -> Result<SubtreeScanDatabaseFacts, SourceDbError> {
        validate_scopes(scopes)?;
        let tx = self.connection.unchecked_transaction()?;
        if read_bounded_numeric_metadata(&tx, "revision")? != revision {
            return Err(SourceDbError::Unexpected);
        }
        let index_revision =
            read_bounded_numeric_metadata(&tx, super::super::META_SOURCE_INDEX_REVISION)?;
        let (generation, expected_count, published_revision) = active(&tx, revision)?;
        let policy = super::super::exact_scan::read_policy(&tx)?;
        let mut facts = SubtreeScanDatabaseFacts {
            binding: self.index_page_binding.clone(),
            revision,
            index_revision,
            generation,
            expected_count,
            published_revision,
            scopes: scopes.to_vec(),
            audio: Vec::new(),
            index: Vec::new(),
            directories: Vec::new(),
            policy,
        };
        let mut count = 0usize;
        let mut bytes = 0usize;
        for scope in scopes {
            for table in [
                "wav_files",
                "source_index_entries",
                "source_directory_entries",
            ] {
                let key = if table == "wav_files" {
                    match scope.as_path().to_str() {
                        Some(_) => super::super::normalize_relative_path(scope.as_path())?,
                        None => continue,
                    }
                } else {
                    normalize_source_index_path(scope.as_path())?.0
                };
                let (lower, upper) = exact_subtree_path_bounds(&key);
                let encoding = if table == "wav_files" {
                    "0"
                } else {
                    "path_encoding"
                };
                let identity = if table == "source_directory_entries" {
                    "CASE WHEN typeof(directory_identity)='text' AND length(CAST(directory_identity AS BLOB)) BETWEEN 1 AND 1024 THEN directory_identity END"
                } else {
                    "NULL"
                };
                let generation_filter = if table == "source_directory_entries" {
                    "AND generation=?4"
                } else {
                    "AND ?4=?4"
                };
                let sql = format!(
                    "SELECT CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND 4096 THEN path END, {encoding}, {identity} FROM {table} WHERE (path=?1 COLLATE BINARY OR (path>=?2 COLLATE BINARY AND path<?3 COLLATE BINARY)) {generation_filter} ORDER BY path COLLATE BINARY LIMIT 129"
                );
                let mut stmt = tx.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params![key, lower, upper, generation])?;
                while let Some(row) = rows.next()? {
                    count += 1;
                    if count > SUBTREE_SCAN_MAX_RETAINED {
                        return Err(SourceDbError::Unexpected);
                    }
                    let key = row
                        .get::<_, Option<String>>(0)?
                        .ok_or(SourceDbError::Unexpected)?;
                    let encoding = row.get::<_, i64>(1)?;
                    let path = if table == "wav_files" {
                        parse_relative_path_from_db(&key)?
                    } else {
                        parse_canonical_directory_path_from_db(&key, encoding)?
                    };
                    bytes = bytes.saturating_add(path.as_os_str().as_encoded_bytes().len());
                    if !path.starts_with(scope.as_path())
                        || path.as_os_str().as_encoded_bytes().len() > 1024
                        || path.components().count() > 32
                        || bytes > 32768
                    {
                        return Err(SourceDbError::Unexpected);
                    }
                    match table {
                        "wav_files" => facts.audio.push(path),
                        "source_index_entries" => facts.index.push(path),
                        _ => {
                            let identity = row
                                .get::<_, Option<String>>(2)?
                                .ok_or(SourceDbError::Unexpected)?;
                            super::super::directories::validate_directory_identity(&identity)?;
                            bytes = bytes.saturating_add(identity.len());
                            if bytes > 32768 {
                                return Err(SourceDbError::Unexpected);
                            }
                            facts.directories.push(SourceDirectoryEntry {
                                relative_path: path,
                                directory_identity: identity,
                            });
                        }
                    }
                }
            }
        }
        Ok(facts)
    }
}
