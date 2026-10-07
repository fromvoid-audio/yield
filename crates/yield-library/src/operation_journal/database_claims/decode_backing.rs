//! Count/layout-derived original binary decoding and validation backing.
//! Call only while original syntax/header scratch is already prepaid.
use super::*;
use std::mem::size_of;

fn overflow() -> DatabaseClaimError {
    DatabaseClaimError("metadata backing overflow")
}
fn add(total: &mut usize, count: usize, each: usize) -> Result<()> {
    *total = total
        .checked_add(count.checked_mul(each).ok_or_else(overflow)?)
        .ok_or_else(overflow)?;
    Ok(())
}
fn node<K, V>() -> usize {
    12 * size_of::<(K, V)>() + 16 * size_of::<usize>()
}
fn text(r: &mut Reader<'_>, total: &mut usize) -> Result<()> {
    let length = r.u16()? as usize;
    require(
        length > 0 && length <= MAX_SOURCE_IDENTITY_BYTES,
        "identity bound",
    )?;
    std::str::from_utf8(r.take(length)?).map_err(|_| DatabaseClaimError("identity UTF-8"))?;
    *total = total.checked_add(length).ok_or_else(overflow)?;
    Ok(())
}

pub(in crate::operation_journal) fn decode_backing_bytes(bytes: &[u8]) -> Result<usize> {
    require(
        bytes.len() <= MAX_INVENTORY_BYTES,
        "aggregate encoding bound",
    )?;
    let mut r = Reader(bytes);
    let magic = r.take(6)?;
    let registered = magic == b"YWCB\x04\x00";
    let forward = registered || magic == b"YWCB\x03\x00";
    let version2 = forward || magic == b"YWCB\x02\x00";
    require(
        version2 || magic == b"YWCB\x01\x00",
        "inventory magic/version",
    )?;
    let mut strings = 0usize;
    text(&mut r, &mut strings)?;
    text(&mut r, &mut strings)?;
    r.take(8 + 16)?;
    if registered {
        r.take(8)?;
    }
    let bindings = r.u16()? as usize;
    let slices = r.u16()? as usize;
    require(
        bindings > 0 && bindings <= MAX_DATABASES && slices > 0 && slices <= MAX_SLICES,
        "inventory count",
    )?;
    let mut participants = 0usize;
    let mut scope_heap = 0usize;
    let mut scope_peak = 0usize;
    for _ in 0..bindings {
        r.take(16)?;
        let role = r.u8()?;
        require(matches!(role, 1 | 2), "role tag")?;
        r.take(8 + 8)?;
        text(&mut r, &mut strings)?;
        r.take(16 + 8 + 8)?;
        text(&mut r, &mut strings)?;
        text(&mut r, &mut strings)?;
        if role == 1 {
            text(&mut r, &mut strings)?;
            text(&mut r, &mut strings)?;
            text(&mut r, &mut strings)?;
            r.take(8)?;
            if version2 {
                let length =
                    u32::from_le_bytes(r.take(4)?.try_into().expect("scope length")) as usize;
                require(
                    length <= super::super::source_lease_binding::MAX_SOURCE_LEASE_SCOPE_BYTES,
                    "lease scope bound",
                )?;
                if length > 0 {
                    let scope = r.take(length)?;
                    let (text, entries, arrays) =
                        super::super::codec::historical_observation::closed_streaming_shape(scope)
                            .ok_or(DatabaseClaimError("lease scope image"))?;
                    // This entire schema is streaming: seven owned identity Strings
                    // and one byte Vec, plus inline fixed arrays/closed scalar enums.
                    // Count every array/text field conservatively, including bad shapes.
                    let heap = text
                        .checked_mul(2)
                        .and_then(|n| entries.checked_mul(2).and_then(|v| n.checked_add(v)))
                        .and_then(|n| arrays.checked_mul(8).and_then(|v| n.checked_add(v)))
                        .ok_or_else(overflow)?;
                    scope_heap = scope_heap.checked_add(heap).ok_or_else(overflow)?;
                    // Existing fence encoder framing (4096), two-byte identity
                    // escaping and four-byte byte-array elements. No new wire cap.
                    let canonical = 4096usize
                        .checked_add(text.checked_mul(2).ok_or_else(overflow)?)
                        .and_then(|n| entries.checked_mul(4).and_then(|v| n.checked_add(v)))
                        .ok_or_else(overflow)?;
                    let peak = heap
                        .checked_mul(2)
                        .and_then(|n| canonical.checked_mul(2).and_then(|v| n.checked_add(v)))
                        // serde's original unknown-field reason may Debug-escape
                        // text, then grow its String. It dies before the static error.
                        .and_then(|n| text.checked_mul(12).and_then(|v| n.checked_add(v)))
                        .and_then(|n| n.checked_add(1024))
                        .ok_or_else(overflow)?;
                    scope_peak = scope_peak.max(peak);
                }
            }
        }
        r.take(8)?;
        let count = r.u16()? as usize;
        participants = participants.checked_add(count).ok_or_else(overflow)?;
        require(
            count > 0 && participants <= MAX_PARTICIPANTS,
            "total participant bound",
        )?;
        r.take(
            count
                .checked_mul(16 + 1 + 1 + 32 + 8 + 8)
                .ok_or_else(overflow)?,
        )?;
    }
    r.take(
        slices
            .checked_mul(16 * 4 + 2 + 8 * 3)
            .ok_or_else(overflow)?,
    )?;
    require(r.0.is_empty(), "trailing inventory bytes")?;
    bound(
        bytes.len(),
        bindings,
        slices,
        participants,
        strings,
        scope_heap,
        scope_peak,
    )
}

/// Malformed input may fail before its final counts are observable. Fund the
/// existing finite partial decoder, then return its unchanged original error.
pub(in crate::operation_journal) fn partial_decode_backing_bytes(wire: usize) -> Result<usize> {
    let mut heap = 0usize;
    // Identities are substrings of this wire; a streaming byte vector has at
    // most one element per raw byte, plus its pinned eight-byte minimum per
    // source binding. No Value trees are constructed by these closed schemas.
    add(&mut heap, wire, 2)?;
    add(&mut heap, wire, 2)?;
    add(&mut heap, MAX_DATABASES, 8)?;
    let mut peak = heap.checked_mul(2).ok_or_else(overflow)?;
    // Existing fence encoder framing, identity escaping and byte elements;
    // geometric output growth and the original serde unknown-field reason.
    add(&mut peak, 2, 4096)?;
    add(&mut peak, wire, 2 * (2 + 4))?;
    add(&mut peak, wire, 12)?;
    add(&mut peak, 1, 1024)?;
    bound(
        wire,
        MAX_DATABASES,
        MAX_SLICES,
        MAX_PARTICIPANTS,
        wire,
        heap,
        peak,
    )
}

fn bound(
    wire: usize,
    bindings: usize,
    slices: usize,
    participants: usize,
    strings: usize,
    scope_heap: usize,
    scope_peak: usize,
) -> Result<usize> {
    let mut total = size_of::<DatabaseClaimInventory>();
    // Owned decoder vectors coexist with stable-sort staging of the same type.
    add(&mut total, bindings, 2 * size_of::<DatabaseBinding>())?;
    add(&mut total, slices, 2 * size_of::<DatabaseClaimSlice>())?;
    add(
        &mut total,
        participants,
        2 * size_of::<DatabaseParticipant>(),
    )?;
    add(&mut total, strings, 2)?;
    total = total
        .checked_add(scope_heap)
        .and_then(|n| n.checked_add(scope_peak))
        .ok_or_else(overflow)?;
    // Original binary readback verification holds expected encode and digest
    // encode concurrently. Each original encoder reserves its exact wire length.
    add(&mut total, wire, 2)?;
    // Validation indices and per-lease selected-slice Vec grow before validity
    // is established; their counts are actual decoded rows, not a new ceiling.
    add(
        &mut total,
        bindings,
        node::<Uuid, ()>() + node::<(u64, &str), ()>(),
    )?;
    add(&mut total, participants, node::<Uuid, ()>())?;
    add(
        &mut total,
        slices,
        3 * node::<Uuid, ()>() + 4 * size_of::<&DatabaseClaimSlice>(),
    )?;
    // Borrowed storage verification formats UUID/device scalars one at a time.
    total
        .checked_add(8 * (36 + size_of::<String>()) + 2 * 32)
        .ok_or_else(overflow)
}
