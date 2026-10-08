//! Fixed original attempt scalar cells; validation remains at the original boundary.
use rusqlite::{Row, types::ValueRef};
use uuid::Uuid;

pub(super) struct UuidProjection {
    pub(super) id: Result<Uuid, uuid::Error>,
    pub(super) canonical: bool,
}
const _: () = assert!(std::mem::size_of::<Option<UuidProjection>>() <= 72);
impl UuidProjection {
    pub(super) fn new(text: &str) -> Self {
        let id = Uuid::parse_str(text);
        let mut encoded = [0; 36];
        let canonical = id
            .as_ref()
            .is_ok_and(|id| id.hyphenated().encode_lower(&mut encoded) == text);
        Self { id, canonical }
    }
    pub(super) fn matches(&self, expected: Uuid) -> bool {
        self.canonical && self.id.as_ref().is_ok_and(|id| *id == expected)
    }
}
fn borrowed_uuid(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<UuidProjection>> {
    if let ValueRef::Text(bytes) = row.get_ref(index)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(Some(UuidProjection::new(text)));
        }
    }
    Ok(None)
}
pub(super) fn required_uuid(row: &Row<'_>, index: usize) -> rusqlite::Result<UuidProjection> {
    if let Some(value) = borrowed_uuid(row, index)? {
        return Ok(value);
    }
    // Preserve the original required String NULL/type/UTF8 diagnostics before decoding.
    copied_required_text(row, index).map(|text| UuidProjection::new(&text))
}
pub(super) fn nullable_uuid(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<UuidProjection>> {
    if let Some(value) = borrowed_uuid(row, index)? {
        return Ok(Some(value));
    }
    copied_nullable_text(row, index).map(|text| text.map(|text| UuidProjection::new(&text)))
}
pub(super) fn copied_required_text(row: &Row<'_>, index: usize) -> rusqlite::Result<String> {
    let text = row.get::<_, String>(index)?;
    #[cfg(test)]
    super::SCALAR_COPIED_BYTES.with(|bytes| bytes.set(bytes.get() + text.len()));
    Ok(text)
}
pub(super) fn copied_nullable_text(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<String>> {
    let text = row.get::<_, Option<String>>(index)?;
    #[cfg(test)]
    super::SCALAR_COPIED_BYTES
        .with(|bytes| bytes.set(bytes.get() + text.as_ref().map_or(0, String::len)));
    Ok(text)
}
#[cfg(test)]
mod tests;
