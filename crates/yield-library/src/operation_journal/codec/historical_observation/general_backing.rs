//! Structure-derived backing for the unchanged non-V1/V2 decoder and classifier.
use super::*;
use crate::operation_journal::{
    coordinator::{UniversalCommandArgs, WaveformEditFrameRange},
    decoded_export_batch_commands::DecodedExportBatchOutput,
    external_commands::{ExternalRawObservation, ExternalRawObservedPath},
    full_capacity::FullCapacityAllocation,
    history_commands::{HistoryAction, HistoryFolderRelocation},
    participants::{ParticipantCheckpoint, ParticipantDescriptor},
};

fn checked(value: Option<usize>) -> Result<usize, OperationJournalStoreError> {
    value.ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}

// Closed stored argument families: capacity and participant arrays, history actions/moves,
// external event/path arrays, decoded output/range arrays and primitive/JSON vectors.
// Include the complete tagged argument union too, so a stored enum's widest variant is
// charged even when its selected representation is a short scalar. Every slot prepays
// actual Rust layout; this is deliberately conservative for primitive byte/unit arrays.
pub(super) fn typed_slot() -> usize {
    [
        size_of::<UniversalCommandArgs>(),
        size_of::<FullCapacityAllocation>(),
        size_of::<crate::durable_capacity::DurableVolumeCapacity>(),
        size_of::<ParticipantDescriptor>(),
        size_of::<ParticipantCheckpoint>(),
        size_of::<HistoryAction>(),
        size_of::<HistoryFolderRelocation>(),
        size_of::<ExternalRawObservation>(),
        size_of::<ExternalRawObservedPath>(),
        size_of::<DecodedExportBatchOutput>(),
        size_of::<WaveformEditFrameRange>(),
        size_of::<Value>(),
    ]
    .into_iter()
    .max()
    .expect("closed typed layouts")
}

impl Scratch<'_> {
    /// Call only after `observe` validated original syntax and duplicate keys.
    /// No graph or second original BLOB is owned while deriving/reserving this loan.
    pub(in crate::operation_journal) fn fund_general(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), OperationJournalStoreError> {
        let inventory = scan::scan_full(bytes);
        let envelope = scan::scan(bytes, |_| {});
        // Pinned BTreeMap full internal nodes, including an empty-object root.
        // IndexMap preserve_order also fits the wider dense/hash/control bound.
        let tree_node =
            12 * size_of::<(String, Value)>() + 12 * size_of::<usize>() + 4 * size_of::<usize>();
        let index_node = 4 * (size_of::<(String, Value)>() + 2 * size_of::<usize>() + 1) + 32;
        let object_node = tree_node.max(index_node);
        let object_bytes = checked(
            inventory
                .keys
                .checked_add(inventory.objects)
                .and_then(|n| n.checked_mul(object_node)),
        )?;
        // Vec geometric growth, including its minimum initial four slots.
        let vector_slots = checked(inventory.array_entries.checked_mul(2).and_then(|n| {
            inventory
                .arrays
                .checked_mul(4)
                .and_then(|a| n.checked_add(a))
        }))?;
        let graph = checked(
            vector_slots
                .checked_mul(size_of::<Value>())
                .and_then(|n| n.checked_add(object_bytes))
                .and_then(|n| {
                    inventory
                        .nodes
                        .checked_mul(size_of::<Value>())
                        .and_then(|v| n.checked_add(v))
                })
                .and_then(|n| {
                    inventory
                        .string_bytes
                        .checked_mul(2)
                        .and_then(|s| n.checked_add(s))
                }),
        )?;
        let typed = checked(
            vector_slots
                .checked_mul(typed_slot())
                .and_then(|n| {
                    inventory
                        .string_bytes
                        .checked_mul(2)
                        .and_then(|s| n.checked_add(s))
                })
                .and_then(|n| n.checked_add(2 * size_of::<StoredOperationRecord>())),
        )?;
        // Opaque payload text is never echoed by the general schema/classifier errors.
        // Envelope text may be Debug-escaped (six bytes per decoded byte), then owned
        // by decoder and retained Store diagnostics at once, each with String growth.
        // The existing fixed recursion/header allowance also prepays fixed framing.
        let errors = checked(
            envelope
                .string_bytes
                .checked_mul(6)
                .and_then(|n| n.checked_add(FIXED_BYTES))
                .and_then(|n| n.checked_mul(4)),
        )?;
        // Initial unique Value graph, nested owned Value deserialization staging and
        // retained payload/field conversion can overlap. Typed arrays are additional.
        // Parser escaped-string scratch remains prepaid throughout these phases.
        let decoding = checked(
            bytes
                .len()
                .checked_mul(2)
                .and_then(|n| graph.checked_mul(3).and_then(|g| n.checked_add(g)))
                .and_then(|n| n.checked_add(typed))
                .and_then(|n| n.checked_add(errors))
                .and_then(|n| n.checked_add(FIXED_BYTES)),
        )?;
        // The pure capacity/participant/command validators retain their decoded record
        // and build identity/volume indices. A full JSON-sized internal node per array
        // entry dominates their smaller keys/values without assuming dense occupancy.
        let classification = checked(
            inventory
                .array_entries
                .checked_mul(object_node)
                .and_then(|n| n.checked_add(typed))
                .and_then(|n| n.checked_add(graph))
                .and_then(|n| n.checked_add(errors))
                .and_then(|n| n.checked_add(FIXED_BYTES)),
        )?;
        self.ensure(decoding.max(classification))
    }
}
