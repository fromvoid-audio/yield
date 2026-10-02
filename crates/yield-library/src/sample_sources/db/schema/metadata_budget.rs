//! Finite compiled-schema validation for the capped metadata transaction policy.
use super::super::{SourceDbError, util::map_sql_error};
use rusqlite::Connection;
use std::sync::OnceLock;

const MAX_OBJECTS: usize = 512;
const MAX_SQL_BYTES: usize = 16384;
const MAX_IMAGE_BYTES: usize = 256 * 1024;

type SchemaImage = Vec<(String, String, String, Option<String>)>;

/// Reject unknown objects/SQL, triggers, attached/temp schemas and dynamic extension callbacks.
pub(in crate::sample_sources::db) fn require_known_metadata_schema(
    connection: &Connection,
) -> Result<(), SourceDbError> {
    if crate::sqlite_ext::extension_load_attempted() {
        return Err(SourceDbError::Unexpected);
    }
    let attached: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name NOT IN ('main','temp'))",
            [],
            |r| r.get(0),
        )
        .map_err(map_sql_error)?;
    let temp_objects: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| r.get(0))
        .map_err(map_sql_error)?;
    // SQLite may install the empty temp namespace while inspecting it. Only main and temp exist.
    if attached || temp_objects != 0 {
        return Err(SourceDbError::Unexpected);
    }
    static EXPECTED: OnceLock<Option<(SchemaImage, Vec<String>)>> = OnceLock::new();
    let (expected, runtime) = EXPECTED
        .get_or_init(|| reference_image().ok())
        .as_ref()
        .ok_or(SourceDbError::Unexpected)?;
    let actual_runtime = runtime_image(connection)?;
    if &actual_runtime != runtime {
        return Err(SourceDbError::Unexpected);
    }
    let actual_schema = image(connection)?;
    if &actual_schema != expected {
        return Err(SourceDbError::Unexpected);
    }
    Ok(())
}

/// Fresh bundled SQLite reference only: no pathname, external source owner or write authority.
fn reference_image() -> Result<(SchemaImage, Vec<String>), SourceDbError> {
    let reference = Connection::open_in_memory().map_err(map_sql_error)?;
    super::assure_schema(&reference)?;
    reference
        .execute_batch(super::super::source_lease::LEASE_TABLE_SQL)
        .map_err(map_sql_error)?;
    Ok((image(&reference)?, runtime_image(&reference)?))
}

fn image(connection: &Connection) -> Result<SchemaImage, SourceDbError> {
    let mut query = connection
        .prepare(
            "SELECT CASE WHEN length(CAST(type AS BLOB))<=16 THEN type ELSE NULL END,
                CASE WHEN length(CAST(name AS BLOB))<=128 THEN name ELSE NULL END,
                CASE WHEN length(CAST(tbl_name AS BLOB))<=128 THEN tbl_name ELSE NULL END,
                CASE WHEN sql IS NULL OR length(CAST(sql AS BLOB))<=16384 THEN sql ELSE NULL END,
                sql IS NOT NULL AND length(CAST(sql AS BLOB))>16384
         FROM sqlite_master ORDER BY type,name LIMIT 513",
        )
        .map_err(map_sql_error)?;
    let mut rows = query.query([]).map_err(map_sql_error)?;
    let mut result = Vec::new();
    let mut bytes = 0_usize;
    while let Some(row) = rows.next().map_err(map_sql_error)? {
        if result.len() == MAX_OBJECTS || row.get::<_, bool>(4).map_err(map_sql_error)? {
            return Err(SourceDbError::Unexpected);
        }
        let kind: String = row.get(0).map_err(map_sql_error)?;
        let name: String = row.get(1).map_err(map_sql_error)?;
        let table: String = row.get(2).map_err(map_sql_error)?;
        let sql: Option<String> = row.get(3).map_err(map_sql_error)?;
        if kind == "trigger" || sql.as_ref().is_some_and(|v| v.len() > MAX_SQL_BYTES) {
            return Err(SourceDbError::Unexpected);
        }
        bytes = bytes
            .checked_add(
                kind.len() + name.len() + table.len() + sql.as_ref().map_or(0, String::len),
            )
            .ok_or(SourceDbError::Unexpected)?;
        if bytes > MAX_IMAGE_BYTES {
            return Err(SourceDbError::Unexpected);
        }
        result.push((kind, name, table, sql));
    }
    Ok(result)
}

fn runtime_image(connection: &Connection) -> Result<Vec<String>, SourceDbError> {
    // SQLite lazily registers its own eponymous pragma modules when queried. Prewarm the
    // finite read-only set used by source maintenance/inspection on both connections, so
    // prior owned observations do not look like an unknown extension. Never allow a prefix.
    for table in [
        "pragma_database_list",
        "pragma_table_info",
        "pragma_table_xinfo",
        "pragma_index_info",
        "pragma_index_xinfo",
        "pragma_index_list",
        "pragma_foreign_key_list",
        "pragma_function_list",
        "pragma_module_list",
        "pragma_collation_list",
    ] {
        connection
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
            .map_err(map_sql_error)?;
    }
    let mut result = Vec::new();
    for sql in [
        "SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name ELSE NULL END,
         builtin,type,enc,narg,flags FROM pragma_function_list
         ORDER BY name,builtin,type,enc,narg,flags LIMIT 513",
        "SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name ELSE NULL END,
         0,'module','',0,0 FROM pragma_module_list ORDER BY name LIMIT 513",
        "SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name ELSE NULL END,
         0,'collation','',0,0 FROM pragma_collation_list ORDER BY name LIMIT 513",
    ] {
        let mut statement = connection.prepare(sql).map_err(map_sql_error)?;
        let mut rows = statement.query([]).map_err(map_sql_error)?;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(map_sql_error)? {
            if count == MAX_OBJECTS {
                return Err(SourceDbError::Unexpected);
            }
            count += 1;
            let name: String = row.get(0).map_err(map_sql_error)?;
            let builtin: i64 = row.get(1).map_err(map_sql_error)?;
            let kind: String = row.get(2).map_err(map_sql_error)?;
            let encoding: String = row.get(3).map_err(map_sql_error)?;
            let arguments: i64 = row.get(4).map_err(map_sql_error)?;
            let flags: i64 = row.get(5).map_err(map_sql_error)?;
            if kind.len() > 16 || encoding.len() > 16 {
                return Err(SourceDbError::Unexpected);
            }
            result.push(format!(
                "{name:?}:{builtin}:{kind}:{encoding}:{arguments}:{flags}"
            ));
        }
    }
    Ok(result)
}
