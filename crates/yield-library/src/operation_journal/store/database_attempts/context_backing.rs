//! Same historical clone/expected-start producer, prepaid under a borrowed record.
use super::*;
use crate::operation_journal::owner::InputPermit;
use database_claims::command_backing::{
    reconstruction_shape, serialized_length, shape_working_bytes,
};
use std::mem::size_of;

fn checked(bytes: Option<usize>) -> Result<usize> {
    bytes.ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}

pub(in crate::operation_journal::store) fn validate(
    start: &DatabaseAttemptStart,
    record: &StoredOperationRecord,
    inventory: &database_claims::RetainedDatabaseInventory,
    new_start: bool,
    input: &InputPermit,
) -> Result<()> {
    let working = input.reserve_historical_scratch(checked(
        super::super::database_attempt_metadata::plan_backing(record)?
            .checked_add(shape_working_bytes())
            .and_then(|n| {
                n.checked_add(4 * size_of::<InputPermit>() + size_of::<DatabaseAttemptStart>())
            }),
    )?)?;
    let mut producer = None;
    let result = (|| {
        let (wire, decode) = inventory
            .reconstruction_backing()
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let phase = if new_start { record.phase } else { start.phase };
        let disposition = if new_start {
            record.disposition
        } else {
            start.disposition
        };
        let shape = reconstruction_shape(record, phase, disposition)?;
        // Closed json! wrapper: actual compact family plus bounded schema/u8,
        // participant UUID and longest existing direction spelling.
        let command = checked(
            shape.arguments.checked_add(
                concat!(
                    "{\"schema\":4294967295,\"requirement\":255,\"participant\":",
                    "\"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx\",",
                    "\"direction\":\"RequiredCompensation\",\"family\":}"
                )
                .len(),
            ),
        )?;
        const FIELDS: [&str; 20] = [
            "version",
            "attempt",
            "nonce",
            "operation",
            "binding",
            "participant",
            "database_slice",
            "sidecar_slice",
            "inventory",
            "schema",
            "phase",
            "disposition",
            "command",
            "command_digest",
            "revision_domain",
            "before",
            "after",
            "database_bytes",
            "wal_bytes",
            "shm_bytes",
        ];
        let framing = FIELDS.iter().map(|field| field.len()).sum::<usize>()
            + 4 * FIELDS.len()
            + 2
            + 7 * 38
            + 5 * 20
            + 10
            + 3
            + 32 * 4
            + 2
            + "\"SourceLeaseLifecycle\"".len();
        let phase_bytes = serialized_length(&phase)?;
        let disposition_bytes = serialized_length(&disposition)?;
        let start_bytes = checked(
            wire.checked_mul(4)
                .and_then(|n| n.checked_add(2 + framing))
                .and_then(|n| n.checked_add(command))
                .and_then(|n| n.checked_add(phase_bytes))
                .and_then(|n| n.checked_add(disposition_bytes)),
        )?;
        // One full internal node per fixed wrapper key/root; preserve_order's
        // alternative layout is covered by the same wider pinned bound.
        let tree = 12 * size_of::<(String, serde_json::Value)>() + 16 * size_of::<usize>();
        let index =
            4 * (size_of::<(String, serde_json::Value)>() + 2 * size_of::<usize>() + 1) + 32;
        let wrapper = 6 * tree.max(index)
            + concat!(
                "schema",
                "requirement",
                "participant",
                "direction",
                "family"
            )
            .len()
                * 2
            + 2 * 36
            + 2 * "RequiredCompensation".len()
            + 6 * size_of::<serde_json::Value>();
        // Original root inventory remains separately charged. This loan owns
        // temporary historical clone, sequential digest producers, two family
        // Value graphs, expected binary inventory/decode and simultaneous outputs.
        let mut needed = 0usize;
        for bytes in [
            shape.cloned,
            shape.reasons,
            wrapper,
            decode,
            checked(shape.output.max(4096).checked_mul(2))?,
            checked(shape.output.checked_mul(2))?,
            checked(shape.arguments.checked_mul(2))?,
            checked(shape.graph.checked_mul(2))?,
            checked(wire.checked_mul(2))?,
            checked(command.max(4096).checked_mul(2))?,
            checked(start_bytes.max(4096).checked_mul(2))?,
        ] {
            needed = checked(needed.checked_add(bytes))?;
        }
        producer = Some(input.reserve_historical_scratch(needed)?);
        // Preserve the original clone, phase precedence, inventory/digests,
        // canonical expected start and exact equality. No authority from counters.
        start
            .validate_bound(record, inventory, new_start)
            .map_err(error)
    })();
    if result.is_err() {
        input.retain_historical_error(working, producer, None, None);
    }
    result
}
