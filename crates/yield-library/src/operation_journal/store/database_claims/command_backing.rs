//! Inventory canonical output before the original digest producer owns bytes/graphs.
use super::*;
use crate::operation_journal::{StoredOperationRecord, codec, owner::InputPermit};
use std::{
    io::{self, Write},
    mem::size_of,
};

#[derive(Clone, Copy, Default)]
struct Frame {
    array: bool,
    key: bool,
}
// Original serde reader accepts at most 128 nesting levels; an extra envelope
// and tagged serializers still fit this checked static stack. No heap growth.
const DEPTH: usize = 256;
struct Shape {
    bytes: usize,
    keys: usize,
    objects: usize,
    arrays: usize,
    entries: usize,
    nodes: usize,
    strings: usize,
    stack: [Frame; DEPTH],
    depth: usize,
    string: bool,
    escape: bool,
    word: bool,
}
impl Shape {
    fn new() -> Self {
        Self {
            bytes: 0,
            keys: 0,
            objects: 0,
            arrays: 0,
            entries: 0,
            nodes: 0,
            strings: 0,
            stack: [Frame::default(); DEPTH],
            depth: 0,
            string: false,
            escape: false,
            word: false,
        }
    }
    fn increment(value: &mut usize) -> io::Result<()> {
        *value = value.checked_add(1).ok_or(io::ErrorKind::OutOfMemory)?;
        Ok(())
    }
    fn value(&mut self) -> io::Result<()> {
        Self::increment(&mut self.nodes)?;
        if self.depth > 0 && self.stack[self.depth - 1].array {
            Self::increment(&mut self.entries)?;
        }
        Ok(())
    }
    fn byte(&mut self, byte: u8) -> io::Result<()> {
        if self.string {
            if self.escape {
                Self::increment(&mut self.strings)?;
                self.escape = false;
            } else if byte == b'"' {
                self.string = false;
            } else {
                Self::increment(&mut self.strings)?;
                if byte == b'\\' {
                    self.escape = true;
                }
            }
            return Ok(());
        }
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => self.word = false,
            b'"' => {
                if self.depth > 0
                    && !self.stack[self.depth - 1].array
                    && self.stack[self.depth - 1].key
                {
                    Self::increment(&mut self.keys)?;
                } else {
                    self.value()?;
                }
                self.string = true;
            }
            b'{' | b'[' => {
                self.value()?;
                if self.depth == DEPTH {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let array = byte == b'[';
                if array {
                    Self::increment(&mut self.arrays)?;
                } else {
                    Self::increment(&mut self.objects)?;
                }
                self.stack[self.depth] = Frame { array, key: !array };
                self.depth += 1;
                self.word = false;
            }
            b'}' | b']' => {
                self.depth = self
                    .depth
                    .checked_sub(1)
                    .ok_or(io::ErrorKind::InvalidData)?;
                self.word = false;
            }
            b':' => {
                if self.depth == 0 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.stack[self.depth - 1].key = false;
                self.word = false;
            }
            b',' => {
                if self.depth == 0 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.stack[self.depth - 1].key = !self.stack[self.depth - 1].array;
                self.word = false;
            }
            _ if !self.word => {
                self.value()?;
                self.word = true;
            }
            _ => {}
        }
        Ok(())
    }
    fn graph(&self) -> Option<usize> {
        // Pinned full BTreeMap nodes; preserve_order's dense/hash/control
        // footprint also fits the wider bound. Original strings grow before
        // final ownership; escaped serialized bytes dominate decoded UTF-8.
        let tree = 12 * size_of::<(String, serde_json::Value)>() + 16 * size_of::<usize>();
        let index =
            4 * (size_of::<(String, serde_json::Value)>() + 2 * size_of::<usize>() + 1) + 32;
        let objects = self
            .keys
            .checked_add(self.objects)?
            .checked_mul(tree.max(index))?;
        let slots = self
            .entries
            .checked_mul(2)?
            .checked_add(self.arrays.checked_mul(4)?)?;
        objects
            .checked_add(slots.checked_mul(size_of::<serde_json::Value>())?)?
            .checked_add(self.nodes.checked_mul(size_of::<serde_json::Value>())?)?
            .checked_add(self.strings.checked_mul(2)?)
    }
}
impl Write for Shape {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        for &byte in bytes {
            self.byte(byte)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(in crate::operation_journal::store) fn shape_working_bytes() -> usize {
    2 * size_of::<Shape>() + size_of::<ReconstructionShape>()
}

pub(in crate::operation_journal::store) fn serialized_length(
    value: &impl serde::Serialize,
) -> Result<usize, OperationJournalStoreError> {
    let mut counter = Shape::new();
    serde_json::to_writer(&mut counter, value).map_err(|_| error("command encoding"))?;
    Ok(counter.bytes)
}

pub(in crate::operation_journal::store) struct ReconstructionShape {
    pub(in crate::operation_journal::store) output: usize,
    pub(in crate::operation_journal::store) arguments: usize,
    pub(in crate::operation_journal::store) graph: usize,
    pub(in crate::operation_journal::store) cloned: usize,
    pub(in crate::operation_journal::store) reasons: usize,
}

// Caller prepays original validation map/header scratch before entering. This
// is an inventory of borrowed serializer output, never schema/effect permission.
pub(in crate::operation_journal::store) fn reconstruction_shape(
    record: &StoredOperationRecord,
    phase: crate::operation_journal::OperationPhase,
    disposition: crate::operation_journal::OperationDisposition,
) -> Result<ReconstructionShape, OperationJournalStoreError> {
    reconstruction_shape_inner(record, phase, disposition, false)
}

pub(in crate::operation_journal::store) fn settlement_shape(
    record: &StoredOperationRecord,
) -> Result<ReconstructionShape, OperationJournalStoreError> {
    reconstruction_shape_inner(record, record.phase, record.disposition, true)
}
fn reconstruction_shape_inner(
    record: &StoredOperationRecord,
    phase: crate::operation_journal::OperationPhase,
    disposition: crate::operation_journal::OperationDisposition,
    untyped: bool,
) -> Result<ReconstructionShape, OperationJournalStoreError> {
    let mut original = Shape::new();
    codec::serialize_full_record_inventory(record, phase, disposition, &mut original)
        .map_err(|_| error("unsupported operation codec"))?
        .map_err(|_| error("unsupported operation codec"))?;
    let mut args = Shape::new();
    if let Some(value) = record.universal_command_args() {
        serde_json::to_writer(&mut args, value).map_err(|_| error("command encoding"))?;
    } else if let Some(value) = record.command_args() {
        serde_json::to_writer(&mut args, value).map_err(|_| error("command encoding"))?;
    } else if !untyped {
        return Err(error("untyped legacy command cannot bind database claims"));
    }
    let slots = original
        .entries
        .checked_mul(2)
        .and_then(|n| {
            original
                .arrays
                .checked_mul(4)
                .and_then(|a| n.checked_add(a))
        })
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    let graph = original
        .graph()
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    let cloned = slots
        .checked_mul(codec::historical_observation::typed_array_slot())
        .and_then(|n| {
            original
                .strings
                .checked_mul(2)
                .and_then(|s| n.checked_add(s))
        })
        .and_then(|n| n.checked_add(graph))
        .and_then(|n| n.checked_add(size_of::<StoredOperationRecord>()))
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    Ok(ReconstructionShape {
        output: original.bytes,
        arguments: args.bytes,
        graph,
        cloned,
        reasons: original
            .strings
            .checked_mul(12)
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
    })
}

pub(in crate::operation_journal::store) fn validate_record(
    record: &StoredOperationRecord,
    inventory: &DatabaseClaimInventory,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let rows = record
        .full_capacity_plan
        .as_ref()
        .map_or(0, |p| p.allocations().len())
        + record.capacity_plan.as_ref().map_or(0, |p| p.volumes.len());
    let node = 12 * size_of::<(crate::durable_capacity::VolumeIdentity, (u64, u64, u64))>()
        + 16 * size_of::<usize>();
    let working = input.reserve_historical_scratch(
        rows.checked_mul(node)
            .and_then(|n| {
                n.checked_add(codec::historical_observation::FIXED_BYTES + 2 * size_of::<Shape>())
            })
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
    )?;
    let mut output = None;
    let result = (|| {
        // Preserve original inventory/envelope validation before command hashing.
        // The actual caller holds the original decoded inventory producer loan.
        inventory.validate().map_err(error)?;
        if record.universal_intent.is_none() {
            return Err(error("missing participant envelope"));
        }
        codec::validate_encoding_family(record)
            .map_err(|_| error("unsupported operation codec"))?;
        let mut original = Shape::new();
        codec::serialize_record(record, &mut original)
            .map_err(|_| error("unsupported operation codec"))?
            .map_err(|_| error("unsupported operation codec"))?;
        if original.bytes > crate::operation_journal::MAX_RECORD_BYTES {
            return Err(error("unsupported operation codec"));
        }
        let mut args = Shape::new();
        if let Some(value) = record.universal_command_args() {
            serde_json::to_writer(&mut args, value).map_err(|_| error("command encoding"))?;
        } else if let Some(value) = record.command_args() {
            serde_json::to_writer(&mut args, value).map_err(|_| error("command encoding"))?;
        } else {
            return Err(error("untyped legacy command cannot bind database claims"));
        }
        // Original encoded operation, escaped parser scratch and compact args
        // output can coexist. CappedWriter starts at its pinned 4096-byte
        // minimum; other Vec growth is at most twofold plus its four-byte floor.
        let needed = original
            .bytes
            .max(4096)
            .checked_mul(2)
            .and_then(|n| original.bytes.checked_mul(2).and_then(|b| n.checked_add(b)))
            .and_then(|n| args.bytes.checked_mul(2).and_then(|b| n.checked_add(b)))
            .and_then(|n| original.graph().and_then(|g| n.checked_add(g)))
            .and_then(|n| n.checked_add(4 + codec::historical_observation::FIXED_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        output = Some(input.reserve_historical_scratch(needed)?);
        inventory.validate_record(record).map_err(error)
    })();
    if result.is_err() {
        input.retain_historical_error(working, output, None, None);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serialized_shape_counts_nested_values_across_every_write_boundary() {
        let bytes =
            br#"{"":[],"escaped\\\"key":{"utf8":"\u263a","n":-12.3e+4},"a":[true,null,"",[1,2]]}"#;
        for width in 1..=bytes.len() {
            let mut shape = Shape::new();
            for chunk in bytes.chunks(width) {
                shape.write_all(chunk).unwrap();
            }
            assert_eq!(
                (
                    shape.bytes,
                    shape.keys,
                    shape.objects,
                    shape.arrays,
                    shape.entries,
                    shape.nodes
                ),
                (bytes.len(), 5, 2, 3, 6, 12)
            );
            // Independently decoded JSON verifies escaped/key/value strings never
            // exceed the prepaid serialized-string capacity.
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            fn string_bytes(v: &serde_json::Value) -> usize {
                match v {
                    serde_json::Value::String(s) => s.len(),
                    serde_json::Value::Array(a) => a.iter().map(string_bytes).sum(),
                    serde_json::Value::Object(o) => {
                        o.iter().map(|(k, v)| k.len() + string_bytes(v)).sum()
                    }
                    _ => 0,
                }
            }
            assert!(shape.strings >= string_bytes(&value));
            assert!(shape.graph().unwrap() >= shape.nodes * size_of::<serde_json::Value>());
        }
    }
    #[test]
    fn serialized_shape_checked_stack_and_size_overflow_refuse_without_growth() {
        let mut shape = Shape::new();
        for _ in 0..DEPTH {
            shape.write_all(b"[").unwrap();
        }
        assert!(shape.write_all(b"[").is_err());
        let mut shape = Shape::new();
        shape.bytes = usize::MAX;
        assert!(shape.write_all(b"0").is_err());
        shape.keys = usize::MAX;
        assert!(shape.graph().is_none());
    }
}

// Count the exact borrowed construction inputs before the owner clones them.
// This inventories Rust metadata only; it supplies no operation or admission.
pub(in crate::operation_journal::store) fn construction_bytes(
    borrowed: &impl serde::Serialize,
) -> Result<usize, OperationJournalStoreError> {
    let mut shape = Shape::new();
    serde_json::to_writer(&mut shape, borrowed)
        .map_err(|_| OperationJournalStoreError::HistoricalClaimResourceBound)?;
    let slots = shape
        .entries
        .checked_mul(2)
        .and_then(|n| shape.arrays.checked_mul(4).and_then(|a| n.checked_add(a)))
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    slots
        .checked_mul(codec::historical_observation::typed_array_slot())
        .and_then(|n| shape.graph().and_then(|g| n.checked_add(g)))
        .and_then(|n| shape.strings.checked_mul(2).and_then(|s| n.checked_add(s)))
        .and_then(|n| n.checked_add(size_of::<StoredOperationRecord>()))
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}
