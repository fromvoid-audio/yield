//! SQL preflight of replay coordination metadata before materializing any stored bytes.
use super::super::SourceDbError;
use rusqlite::Connection;

pub(in crate::sample_sources::db) enum MetadataValue {
    Absent,
    Invalid,
    Value(String),
}
pub(in crate::sample_sources::db) fn read_metadata(
    connection: &Connection,
    key: &str,
) -> Result<MetadataValue, SourceDbError> {
    let mut statement = connection.prepare(
        "SELECT CASE WHEN typeof(value) = 'text'
           AND length(CAST(value AS BLOB)) BETWEEN 1 AND 16384 THEN value END
         FROM metadata WHERE key = ?1 LIMIT 2",
    )?;
    let mut rows = statement.query([key])?;
    let Some(row) = rows.next()? else {
        return Ok(MetadataValue::Absent);
    };
    let value = row.get::<_, Option<String>>(0)?;
    if rows.next()?.is_some() {
        return Ok(MetadataValue::Invalid);
    }
    Ok(value.map_or(MetadataValue::Invalid, MetadataValue::Value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sql_preflight_rejects_oversize_types_null_and_duplicates() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE metadata(key TEXT, value)")
            .unwrap();
        assert!(matches!(
            read_metadata(&db, "key").unwrap(),
            MetadataValue::Absent
        ));
        db.execute(
            "INSERT INTO metadata VALUES ('key', ?1)",
            ["x".repeat(16384)],
        )
        .unwrap();
        assert!(
            matches!(read_metadata(&db, "key").unwrap(), MetadataValue::Value(v) if v.len() == 16384)
        );
        for sql in [
            "UPDATE metadata SET value = zeroblob(1000000)",
            "UPDATE metadata SET value = 1",
            "UPDATE metadata SET value = NULL",
            "UPDATE metadata SET value = ''",
            "UPDATE metadata SET value = CAST(zeroblob(1000000) AS TEXT)",
        ] {
            db.execute_batch(sql).unwrap();
            assert!(matches!(
                read_metadata(&db, "key").unwrap(),
                MetadataValue::Invalid
            ));
        }
        db.execute_batch(
            "UPDATE metadata SET value = 'valid'; INSERT INTO metadata VALUES ('key', 'duplicate')",
        )
        .unwrap();
        assert!(matches!(
            read_metadata(&db, "key").unwrap(),
            MetadataValue::Invalid
        ));
    }
}
