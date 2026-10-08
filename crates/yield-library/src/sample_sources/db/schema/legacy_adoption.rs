//! Reviewed additive known-11 migration, without generic cleanup or authored-row rewrites.
//! The original admitted source owner must supply transaction, policy and receipt custody.
use super::*;

pub(in crate::sample_sources::db) fn apply_preserving_legacy_schema(
    connection: &Connection,
) -> Result<(), SourceDbError> {
    let mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(map_sql_error)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sql_error)?;
    let page_size: u64 = connection
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(map_sql_error)?;
    let pages: u64 = connection
        .pragma_query_value(None, "max_page_count", |row| row.get(0))
        .map_err(map_sql_error)?;
    let spill: i64 = connection
        .pragma_query_value(None, "cache_spill", |row| row.get(0))
        .map_err(map_sql_error)?;
    let temp: i64 = connection
        .pragma_query_value(None, "temp_store", |row| row.get(0))
        .map_err(map_sql_error)?;
    if connection.is_autocommit()
        || !mode.eq_ignore_ascii_case("delete")
        || version != 11
        || spill != 0
        || temp != 2
        || !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || pages
            .checked_mul(page_size)
            .is_none_or(|n| n > 64 * 1024 * 1024)
    {
        return Err(SourceDbError::MaintenanceRequired);
    }
    // The actual owner validates the complete original known-11 image before this boundary.
    // Reuse reviewed compiled structural DDL, explicitly omitting data cleanup, canonical
    // identity rewriting, legacy runtime retirement and generic version stamping.
    ddl::apply_base_schema(connection)?;
    migrations::apply_legacy_structural_inside_transaction(connection)?;
    Ok(())
}

// Build a bounded in-memory oracle from the already reviewed original two-table DDL.
// This never opens a physical database or grants an effect.
pub(in crate::sample_sources::db) fn expected_legacy_postimage(
    original: &metadata_budget::SchemaImage,
) -> Result<metadata_budget::SchemaImage, SourceDbError> {
    super::super::legacy_observation::validate_original_schema_image(original)
        .map_err(|_| SourceDbError::Unexpected)?;
    let reference = Connection::open_in_memory().map_err(map_sql_error)?;
    for (kind, name, _, sql) in original {
        if kind == "table" {
            if !matches!(name.as_str(), "metadata" | "wav_files") {
                return Err(SourceDbError::Unexpected);
            }
            reference
                .execute_batch(sql.as_deref().ok_or(SourceDbError::Unexpected)?)
                .map_err(map_sql_error)?;
        }
    }
    reference
        .execute_batch("BEGIN EXCLUSIVE")
        .map_err(map_sql_error)?;
    ddl::apply_base_schema(&reference)?;
    migrations::apply_legacy_structural_inside_transaction(&reference)?;
    metadata_budget::image(&reference)
}

pub(in crate::sample_sources::db) fn legacy_schema_digest(
    image: &metadata_budget::SchemaImage,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for (kind, name, table, sql) in image {
        for text in [
            kind.as_str(),
            name.as_str(),
            table.as_str(),
            sql.as_deref().unwrap_or_default(),
        ] {
            hash.update(&(text.len() as u64).to_le_bytes());
            hash.update(text.as_bytes());
        }
    }
    *hash.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_postimage_oracle_rejects_unreviewed_ddl_before_it_can_open_an_attached_file() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE wav_files (path TEXT PRIMARY KEY, file_size INTEGER NOT NULL, modified_ns INTEGER NOT NULL)").unwrap();
        let original = metadata_budget::image(&connection).unwrap();
        assert!(expected_legacy_postimage(&original).is_ok());
        let mut injected = original.clone();
        let target = root.path().join("unadmitted.sqlite");
        let table = injected
            .iter_mut()
            .find(|entry| entry.1 == "metadata" && entry.0 == "table")
            .unwrap();
        table.3.as_mut().unwrap().push_str(&format!(
            "; ATTACH DATABASE '{}' AS unexpected; CREATE TABLE unexpected.effect(value INTEGER)",
            target.display()
        ));
        assert!(expected_legacy_postimage(&injected).is_err());
        assert!(!target.exists());
        let mut oversized = original;
        let table = oversized
            .iter_mut()
            .find(|entry| entry.1 == "metadata" && entry.0 == "table")
            .unwrap();
        table.3 = Some(format!("{}{}", " ".repeat(4096), table.3.take().unwrap()));
        assert!(expected_legacy_postimage(&oversized).is_err());
    }
    #[test]
    fn legacy_additive_schema_preserves_original_metadata_and_three_column_audio_rows() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open(root.path().join("legacy.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE wav_files (path TEXT PRIMARY KEY, file_size INTEGER NOT NULL, modified_ns INTEGER NOT NULL);
            INSERT INTO metadata VALUES ('revision', '7'), ('original.custom', 'unchanged');
            INSERT INTO wav_files VALUES ('kept.wav', 28, 13), ('folder/kept.FLAC', 44, -17);
            PRAGMA user_version=11; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA max_page_count=16384;").unwrap();
        assert!(apply_preserving_legacy_schema(&connection).is_err());
        connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        apply_preserving_legacy_schema(&connection).unwrap();
        let mut rows = connection
            .prepare("SELECT key,value FROM metadata ORDER BY key")
            .unwrap();
        let metadata = rows
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            metadata,
            vec![
                ("original.custom".into(), "unchanged".into()),
                ("revision".into(), "7".into())
            ]
        );
        drop(rows);
        let mut rows = connection
            .prepare("SELECT path,file_size,modified_ns FROM wav_files ORDER BY path")
            .unwrap();
        let audio = rows
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            audio,
            vec![
                ("folder/kept.FLAC".into(), 44, -17),
                ("kept.wav".into(), 28, 13)
            ]
        );
        drop(rows);
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            11
        );
        connection.execute_batch("ROLLBACK").unwrap();
        assert_eq!(table_columns(&connection, "wav_files").unwrap().len(), 3);
    }
}
