//! Borrow original SQLite scalars without retaining unbounded TEXT.
use rusqlite::{Row, types::ValueRef};
use uuid::Uuid;

pub(super) struct UuidProjection {
    pub(super) id: Result<Uuid, uuid::Error>,
    pub(super) canonical: bool,
}
// A complete fixed-size page cell, including malformed UUID diagnostics. This
// fits the original SQL page allowance without any owned input-string payload.
const _: () = assert!(std::mem::size_of::<Option<UuidProjection>>() <= 72);

impl UuidProjection {
    fn new(text: &str) -> Self {
        let id = Uuid::parse_str(text);
        let mut encoded = [0; 36];
        let canonical = id
            .as_ref()
            .is_ok_and(|id| id.hyphenated().encode_lower(&mut encoded) == text);
        Self { id, canonical }
    }
}

pub(super) fn uuid(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<UuidProjection>> {
    if let ValueRef::Text(bytes) = row.get_ref(index)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            // Defer the result until the original caller's validation point. In
            // particular, later SQL/UTF-8 errors still precede page UUID errors.
            return Ok(Some(UuidProjection::new(text)));
        }
    }
    // The pinned driver's String conversion borrows UTF-8 before copying. NULL,
    // wrong type and invalid UTF-8 take exactly its original conversion/error path.
    copied_text(row, index).map(|text| text.map(|text| UuidProjection::new(&text)))
}

/// Same original nullable String conversion on a funded borrowed statement.
/// Valid TEXT remains borrowed; fallback can succeed only for NULL.
pub(super) fn uuid_value(
    value: ValueRef<'_>,
    index: usize,
    column: &str,
) -> rusqlite::Result<Option<UuidProjection>> {
    if let ValueRef::Text(bytes) = value {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(Some(UuidProjection::new(text)));
        }
    }
    use rusqlite::types::{FromSql, FromSqlError};
    <Option<String> as FromSql>::column_result(value)
        .map(|text| text.map(|text| UuidProjection::new(&text)))
        .map_err(|error| match error {
            FromSqlError::InvalidType => {
                rusqlite::Error::InvalidColumnType(index, column.into(), value.data_type())
            }
            FromSqlError::OutOfRange(value) => {
                rusqlite::Error::IntegralValueOutOfRange(index, value)
            }
            FromSqlError::Other(error) => {
                rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), error)
            }
            error @ FromSqlError::InvalidBlobSize { .. } => {
                rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
            }
            _ => unreachable!("pinned nullable String conversion"),
        })
}

pub(super) fn matches_text(row: &Row<'_>, index: usize, expected: &str) -> rusqlite::Result<bool> {
    if let ValueRef::Text(bytes) = row.get_ref(index)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(text == expected);
        }
    }
    copied_text(row, index).map(|text| text.as_deref() == Some(expected))
}

// Keep the exact pinned driver's nullable String errors. This path can succeed
// only for NULL: valid UTF-8 TEXT already returned through the borrowed branch.
pub(super) fn copied_text(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    let text = row.get::<_, Option<String>>(index)?;
    #[cfg(test)]
    super::SCALAR_COPIED_BYTES.with(|bytes| {
        bytes.set(bytes.get() + text.as_ref().map_or(0, String::len));
    });
    Ok(text)
}

#[cfg(test)]
mod tests;
