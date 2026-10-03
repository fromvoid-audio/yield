//! Exact compiled legacy DDL image; unknown objects are never executed or cleaned up.
use super::{LegacyObservationError, Result, refused};
use rusqlite::Connection;

fn normalized(sql: &str) -> String {
    sql.chars()
        .filter(|c| !c.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
pub(super) fn capture(connection: &Connection) -> Result<[u8; 32]> {
    let version: u64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(refused)?;
    let mode: String = connection
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(refused)?;
    let query_only: u64 = connection
        .pragma_query_value(None, "query_only", |r| r.get(0))
        .map_err(refused)?;
    let temp: u64 = connection
        .pragma_query_value(None, "temp_store", |r| r.get(0))
        .map_err(refused)?;
    let spill: u64 = connection
        .pragma_query_value(None, "cache_spill", |r| r.get(0))
        .map_err(refused)?;
    if version != 11
        || !mode.eq_ignore_ascii_case("delete")
        || query_only != 1
        || temp != 2
        || spill != 0
    {
        return Err(LegacyObservationError(
            "unsupported legacy version/mode or read policy",
        ));
    }
    let mut query = connection.prepare("SELECT CASE WHEN length(CAST(type AS BLOB))<=16 THEN type END, CASE WHEN length(CAST(name AS BLOB))<=128 THEN name END, CASE WHEN length(CAST(tbl_name AS BLOB))<=128 THEN tbl_name END, CASE WHEN sql IS NULL OR length(CAST(sql AS BLOB))<=4096 THEN sql END FROM sqlite_schema LIMIT 5").map_err(refused)?;
    let rows = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(refused)?;
    let mut actual = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(refused)?;
    actual.sort();
    let expected = [
        ("index", "sqlite_autoindex_metadata_1", "metadata", None),
        ("index", "sqlite_autoindex_wav_files_1", "wav_files", None),
        (
            "table",
            "metadata",
            "metadata",
            Some("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)"),
        ),
        (
            "table",
            "wav_files",
            "wav_files",
            Some(
                "CREATE TABLE wav_files (path TEXT PRIMARY KEY, file_size INTEGER NOT NULL, modified_ns INTEGER NOT NULL)",
            ),
        ),
    ];
    if actual.len() != expected.len()
        || actual.iter().zip(expected).any(|(a, e)| {
            a.0 != e.0
                || a.1 != e.1
                || a.2 != e.2
                || a.3.as_deref().map(normalized) != e.3.map(normalized)
        })
    {
        return Err(LegacyObservationError("unknown legacy schema image"));
    }
    let mut digest = blake3::Hasher::new();
    for (kind, name, table, sql) in actual {
        for text in [kind, name, table, sql.unwrap_or_default()] {
            digest.update(&(text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
        }
    }
    Ok(*digest.finalize().as_bytes())
}
