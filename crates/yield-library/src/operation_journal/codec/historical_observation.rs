//! Funded V1/V2 BOOL observation. Only the opaque root payload is discarded.
//! The remaining bounded envelope uses the unchanged strict decoder and classifier.
use super::super::owner::InputPermit;
use super::super::store::OperationJournalStoreError;
use super::{DecodeError, StoredOperationRecord, decode_value, parse_unique_value};
use serde_json::Value;
use std::mem::size_of;

mod keys;
mod scan;
mod syntax;
#[cfg(test)]
mod tests;

// Parser, scalar scanner recursion (at most the original serde 128 nesting limit),
// owned vector headers, projected/typed record headers, and bounded SQL/error context.
pub(super) const FIXED_BYTES: usize = size_of::<
    serde_json::Deserializer<serde_json::de::SliceRead<'static>>,
>() + 128 * (size_of::<keys::Key>() + 8 * size_of::<usize>())
    + 4 * size_of::<Vec<u8>>()
    + 2 * size_of::<StoredOperationRecord>()
    + 2 * size_of::<InputPermit>()
    + size_of::<rusqlite::Statement<'static>>()
    + size_of::<rusqlite::Rows<'static>>()
    + 36
    + 256;

pub(super) struct Scratch<'a> {
    input: &'a InputPermit,
    parser_bytes: usize,
    funded: usize,
    extra: Option<InputPermit>,
    _parser: InputPermit,
}
impl<'a> Scratch<'a> {
    pub(super) fn new(
        input: &'a InputPermit,
        length: usize,
    ) -> Result<Self, OperationJournalStoreError> {
        let parser_bytes = length
            .checked_mul(2)
            .and_then(|n| n.checked_add(FIXED_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let parser = input.reserve_historical_scratch(parser_bytes)?;
        Ok(Self {
            input,
            parser_bytes,
            funded: parser_bytes,
            extra: None,
            _parser: parser,
        })
    }
    fn ensure(&mut self, bytes: usize) -> Result<(), OperationJournalStoreError> {
        if bytes > self.funded {
            drop(self.extra.take());
            self.funded = self.parser_bytes;
            self.extra = Some(
                self.input
                    .reserve_historical_scratch(bytes - self.parser_bytes)?,
            );
            self.funded = bytes;
        }
        Ok(())
    }
}

fn malformed() -> OperationJournalStoreError {
    OperationJournalStoreError::HistoricalClaimObservation {
        reason: "historical JSON or envelope is invalid".into(),
    }
}

pub(super) fn observe(
    bytes: &[u8],
    scratch: &mut Scratch<'_>,
) -> Result<Option<StoredOperationRecord>, OperationJournalStoreError> {
    // serde escaped-string scratch grows geometrically. Two raw-wire bytes per byte
    // conservatively cover its capacity, including the 128-byte initial allocation.
    syntax::validate(bytes).map_err(|_| malformed())?;
    let inventory = scan::scan(bytes, |_| {});
    let key_bytes = inventory
        .keys
        .checked_mul(size_of::<keys::Key>())
        .and_then(|n| n.checked_add(FIXED_BYTES))
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    scratch.ensure(key_bytes)?;
    let mut keys = Vec::with_capacity(inventory.keys);
    scan::scan(bytes, |key| keys.push(key));
    keys.sort_unstable_by(|left, right| keys::compare(left, right, bytes));
    if keys
        .windows(2)
        .any(|pair| keys::compare(&pair[0], &pair[1], bytes).is_eq())
    {
        return Err(malformed());
    }
    drop(keys);
    let version = inventory.version.ok_or_else(malformed)?;
    if !matches!(version, 1 | 2) {
        return Ok(None);
    }
    let payload = inventory.payload.ok_or_else(malformed)?;
    let projected_len = bytes.len() - payload.len() + 4;
    // Pinned Rust BTreeMap: at most eleven pairs and twelve child edges per node.
    // Charge a FULL internal node for EACH entry (including empty-object roots),
    // without assuming sharing/dense occupancy. Array Vec growth is at most twice
    // the element count plus its four-element initial capacity. Typed capacity
    // arrays and fixed record headers are funded simultaneously, not as a reuse.
    let tree_node =
        12 * size_of::<(String, Value)>() + 12 * size_of::<usize>() + 4 * size_of::<usize>();
    let graph_bytes = (inventory.retained_keys + inventory.objects) * tree_node
        + (2 * inventory.array_entries + 4 * inventory.arrays) * size_of::<Value>()
        + inventory.nodes * size_of::<Value>()
        + 2 * inventory.string_bytes;
    let typed_bytes =
        2 * inventory.array_entries * size_of::<crate::durable_capacity::DurableVolumeCapacity>()
            + 4 * size_of::<crate::durable_capacity::DurableVolumeCapacity>()
            + 2 * inventory.string_bytes;
    let envelope_bytes = FIXED_BYTES + 3 * projected_len + graph_bytes + typed_bytes;
    let identity_node = 12 * size_of::<crate::durable_capacity::VolumeIdentity>()
        + 12 * size_of::<usize>()
        + 4 * size_of::<usize>();
    let classification_bytes = FIXED_BYTES + typed_bytes + inventory.array_entries * identity_node;
    scratch.ensure(envelope_bytes.max(classification_bytes))?;
    let mut projected = Vec::with_capacity(projected_len);
    projected.extend_from_slice(&bytes[..payload.start]);
    projected.extend_from_slice(b"null");
    projected.extend_from_slice(&bytes[payload.end..]);
    // The original wire was bounded before projection. Replacing a one-byte scalar
    // with null may add three bytes at the wire boundary; synthetic size is not a
    // new historical compatibility gate. Reuse the unchanged parser and dispatch.
    let value = parse_unique_value(&projected).map_err(|_| malformed())?;
    let record = decode_value(value).map_err(|error| match error {
        DecodeError::Oversize { .. } => OperationJournalStoreError::HistoricalClaimResourceBound,
        _ => malformed(),
    })?;
    Ok(Some(record))
}
